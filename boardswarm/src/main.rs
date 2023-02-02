use boardswarm_protocol::item_event::Event;
use boardswarm_protocol::{
    console_input_request, upload_request, ConsoleConfigureRequest, ConsoleInputRequest,
    ConsoleOutputRequest, ItemEvent, ItemList, ItemTypeRequest, UploaderInfoMsg, UploaderRequest,
};
use bytes::Bytes;
use clap::Parser;
use futures::prelude::*;
use futures::stream::BoxStream;
use futures::Sink;
use registry::{Properties, Registry};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::{net::ToSocketAddrs, sync::Arc};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio_stream::wrappers::WatchStream;
use tonic::Streaming;

use tracing::{info, warn};

use crate::registry::RegistryChange;

mod config;
mod dfu;
mod pdudaemon;
mod registry;
mod serial;
mod udev;

#[derive(Error, Debug)]
#[error("Actuator failed")]
pub struct ActuatorError();

#[async_trait::async_trait]
trait Actuator: std::fmt::Debug + Send + Sync {
    async fn set_mode(
        &self,
        parameters: Box<dyn erased_serde::Deserializer<'static> + Send>,
    ) -> Result<(), ActuatorError>;
}

#[derive(Error, Debug)]
pub enum ConsoleError {}

#[async_trait::async_trait]
trait Console: std::fmt::Debug + Send + Sync {
    fn configure(
        &self,
        parameters: Box<dyn erased_serde::Deserializer>,
    ) -> Result<(), ConsoleError>;
    async fn input(
        &self,
    ) -> Result<Pin<Box<dyn Sink<Bytes, Error = ConsoleError> + Send>>, ConsoleError>;
    async fn output(&self)
        -> Result<BoxStream<'static, Result<Bytes, ConsoleError>>, ConsoleError>;
}

type ConsoleOutputStream =
    stream::BoxStream<'static, Result<boardswarm_protocol::ConsoleOutput, tonic::Status>>;

#[async_trait::async_trait]
trait ConsoleExt: Console {
    async fn output_stream(&self) -> ConsoleOutputStream {
        Box::pin(self.output().await.unwrap().map(|data| {
            Ok(boardswarm_protocol::ConsoleOutput {
                data: data.unwrap(),
            })
        }))
    }
}

impl<C> ConsoleExt for C where C: Console + ?Sized {}

#[derive(Clone, Error, Debug)]
pub enum UploaderError {}

type UploadProgressStream = WatchStream<Result<boardswarm_protocol::UploadProgress, tonic::Status>>;

#[derive(Debug)]
pub struct UploadProgress {
    tx: tokio::sync::watch::Sender<Result<boardswarm_protocol::UploadProgress, tonic::Status>>,
}

impl UploadProgress {
    fn new() -> (Self, UploadProgressStream) {
        let (tx, rx) =
            tokio::sync::watch::channel(Ok(boardswarm_protocol::UploadProgress { written: 0 }));
        (Self { tx }, WatchStream::new(rx))
    }

    fn update(&self, written: u64) {
        let _ = self
            .tx
            .send(Ok(boardswarm_protocol::UploadProgress { written }));
    }
}

#[async_trait::async_trait]
pub trait Uploader: std::fmt::Debug + Send + Sync {
    fn targets(&self) -> &[String];
    async fn upload(
        &self,
        target: &str,
        data: BoxStream<'static, Bytes>,
        length: u64,
        progress: UploadProgress,
    ) -> Result<(), UploaderError>;

    async fn commit(&self) -> Result<(), UploaderError>;
}

trait DeviceConfigItem {
    fn matches(&self, properties: &Properties) -> bool;
}

impl DeviceConfigItem for config::Console {
    fn matches(&self, properties: &Properties) -> bool {
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::Uploader {
    fn matches(&self, properties: &Properties) -> bool {
        properties.matches(&self.match_)
    }
}

impl DeviceConfigItem for config::ModeStep {
    fn matches(&self, properties: &Properties) -> bool {
        properties.matches(&self.match_)
    }
}

struct DeviceItem<C> {
    config: C,
    id: Mutex<Option<u64>>,
}

impl<C> DeviceItem<C>
where
    C: DeviceConfigItem,
{
    fn new(config: C) -> Self {
        Self {
            config,
            id: Mutex::new(None),
        }
    }

    fn config(&self) -> &C {
        &self.config
    }

    fn set(&self, id: Option<u64>) {
        *self.id.lock().unwrap() = id;
    }

    fn get(&self) -> Option<u64> {
        *self.id.lock().unwrap()
    }

    fn unset_if_matches(&self, id: u64) -> bool {
        let mut i = self.id.lock().unwrap();
        match *i {
            Some(item_id) if item_id == id => {
                *i = None;
                true
            }
            _ => false,
        }
    }

    fn set_if_matches(&self, id: u64, properties: &Properties) -> bool {
        if self.config.matches(properties) {
            self.set(Some(id));
            true
        } else {
            false
        }
    }
}

impl From<&Device> for boardswarm_protocol::Device {
    fn from(d: &Device) -> Self {
        let consoles = d
            .inner
            .consoles
            .iter()
            .map(|c| boardswarm_protocol::Console {
                name: c.config().name.clone(),
                id: c.get(),
            })
            .collect();
        let uploaders = d
            .inner
            .uploaders
            .iter()
            .map(|u| boardswarm_protocol::Uploader {
                name: u.config().name.clone(),
                id: u.get(),
            })
            .collect();
        let modes = d
            .inner
            .modes
            .iter()
            .map(|m| boardswarm_protocol::Mode {
                name: m.name.clone(),
                depends: m.depends.clone(),
                available: m.sequence.iter().all(|s| s.get().is_some()),
            })
            .collect();
        let current_mode = d.current_mode();
        boardswarm_protocol::Device {
            consoles,
            uploaders,
            current_mode,
            modes,
        }
    }
}

#[derive(Debug, Error)]
#[error("Device is no longer there")]
struct DeviceGone();

struct DeviceMonitor {
    receiver: broadcast::Receiver<()>,
}

#[derive(Debug, Error)]
enum DeviceSetModeError {
    #[error("Mode not found")]
    ModeNotFound,
    #[error("Wrong current mode")]
    WrongCurrentMode,
    #[error("Actuator failed: {0}")]
    ActuatorFailed(#[from] ActuatorError),
}

impl DeviceMonitor {
    async fn wait(&mut self) -> Result<(), DeviceGone> {
        while let Err(e) = self.receiver.recv().await {
            match e {
                broadcast::error::RecvError::Closed => return Err(DeviceGone()),
                broadcast::error::RecvError::Lagged(_) => continue,
            }
        }
        Ok(())
    }
}

// TODO deal with closing
struct DeviceNotifier {
    sender: broadcast::Sender<()>,
}

impl DeviceNotifier {
    fn new() -> Self {
        Self {
            sender: broadcast::channel(1).0,
        }
    }

    async fn notify(&self) {
        let _ = self.sender.send(());
    }

    fn watch(&self) -> DeviceMonitor {
        DeviceMonitor {
            receiver: self.sender.subscribe(),
        }
    }
}

struct DeviceMode {
    name: String,
    depends: Option<String>,
    sequence: Vec<DeviceItem<config::ModeStep>>,
}

impl From<config::Mode> for DeviceMode {
    fn from(config: config::Mode) -> Self {
        let sequence = config.sequence.into_iter().map(DeviceItem::new).collect();
        DeviceMode {
            name: config.name,
            depends: config.depends,
            sequence,
        }
    }
}

struct DeviceInner {
    notifier: DeviceNotifier,
    name: String,
    current_mode: Mutex<Option<String>>,
    consoles: Vec<DeviceItem<config::Console>>,
    uploaders: Vec<DeviceItem<config::Uploader>>,
    modes: Vec<DeviceMode>,
    server: Server,
}

#[derive(Clone)]
struct Device {
    inner: Arc<DeviceInner>,
}

impl Device {
    fn from_config(config: config::Device, server: Server) -> Device {
        let name = config.name;
        let consoles = config.consoles.into_iter().map(DeviceItem::new).collect();
        let uploaders = config.uploaders.into_iter().map(DeviceItem::new).collect();
        let notifier = DeviceNotifier::new();
        let modes = config.modes.into_iter().map(Into::into).collect();
        Device {
            inner: Arc::new(DeviceInner {
                notifier,
                name,
                current_mode: Mutex::new(None),
                consoles,
                uploaders,
                modes,
                server,
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn updates(&self) -> DeviceMonitor {
        self.inner.notifier.watch()
    }

    // TODO add a semaphore ot only allow one sequence to run at a time
    pub async fn set_mode(&self, mode: &str) -> Result<(), DeviceSetModeError> {
        let target = self
            .inner
            .modes
            .iter()
            .find(|m| m.name == mode)
            .ok_or(DeviceSetModeError::ModeNotFound)?;
        {
            let mut current = self.inner.current_mode.lock().unwrap();
            if let Some(depend) = &target.depends {
                if current.as_ref() != Some(depend) {
                    return Err(DeviceSetModeError::WrongCurrentMode);
                }
            }
            *current = None;
        }

        for step in &target.sequence {
            let step = step.config();
            if let Some(provider) = self.inner.server.find_actuator(&step.match_) {
                provider
                    .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                        step.parameters.clone(),
                    )))
                    .await?;
            } else {
                warn!("Provider {:?} not found", &step.match_);
                return Err(ActuatorError {}.into());
            }
            if let Some(duration) = step.stabilisation {
                tokio::time::sleep(duration).await;
            }
        }
        {
            let mut current = self.inner.current_mode.lock().unwrap();
            *current = Some(mode.to_string());
        }
        self.inner.notifier.notify().await;
        Ok(())
    }

    fn current_mode(&self) -> Option<String> {
        let mode = self.inner.current_mode.lock().unwrap();
        mode.clone()
    }

    async fn monitor_items(&self) {
        fn add_item_with<'a, C, I, F, IT>(
            items: I,
            id: u64,
            properties: &Properties,
            item: IT,
            f: F,
        ) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
            F: Fn(&DeviceItem<C>, &IT),
        {
            items.fold(false, |changed, i| {
                if i.set_if_matches(id, properties) {
                    f(i, &item);
                    true
                } else {
                    changed
                }
            })
        }
        fn add_item<'a, C: DeviceConfigItem + 'a, I: Iterator<Item = &'a DeviceItem<C>>>(
            items: I,
            id: u64,
            properties: &Properties,
        ) -> bool {
            add_item_with(items, id, properties, (), |_, _| {})
        }

        fn change_with<'a, T, C, I, F>(items: I, change: RegistryChange<T>, f: F) -> bool
        where
            C: DeviceConfigItem + 'a,
            I: Iterator<Item = &'a DeviceItem<C>>,
            F: Fn(&DeviceItem<C>, &T),
        {
            match change {
                registry::RegistryChange::Added {
                    id,
                    properties,
                    item,
                } => add_item_with(items, id, &properties, item, f),
                registry::RegistryChange::Removed(id) => {
                    items.fold(false, |changed, c| c.unset_if_matches(id) || changed)
                }
            }
        }
        fn change<'a, T, C: DeviceConfigItem + 'a, I: Iterator<Item = &'a DeviceItem<C>>>(
            items: I,
            change: RegistryChange<T>,
        ) -> bool {
            change_with(items, change, |_, _| {})
        }
        fn setup_console(dev: &DeviceItem<config::Console>, console: &Arc<dyn Console>) {
            if let Err(e) = console.configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                dev.config().parameters.clone(),
            ))) {
                warn!("Failed to configure console: {}", e);
            }
        }

        let mut actuator_monitor = self.inner.server.inner.actuators.monitor();
        let mut console_monitor = self.inner.server.inner.consoles.monitor();
        let mut uploader_monitor = self.inner.server.inner.uploaders.monitor();
        let mut changed = false;

        for (id, properties, ..) in self.inner.server.inner.actuators.contents() {
            changed |= add_item(
                self.inner.modes.iter().flat_map(|m| m.sequence.iter()),
                id,
                &properties,
            );
        }

        for (id, properties, item) in self.inner.server.inner.consoles.contents() {
            changed |= add_item_with(
                self.inner.consoles.iter(),
                id,
                &properties,
                item,
                setup_console,
            );
        }

        for (id, properties, ..) in self.inner.server.inner.uploaders.contents() {
            changed |= add_item(self.inner.uploaders.iter(), id, &properties);
        }

        if changed {
            self.inner.notifier.notify().await;
        }

        loop {
            let changed = tokio::select! {
                msg = console_monitor.recv() => {
                    match msg {
                        Ok(c) => change_with(self.inner.consoles.iter(), c, setup_console),
                        Err(e) => {
                            warn!("Issue with monitoring consoles: {:?}", e); return },
                    }
                }
                msg = actuator_monitor.recv() => {
                    match msg {
                        Ok(c) => change(
                            self.inner.modes.iter().flat_map(|m| m.sequence.iter()),
                            c),
                        Err(e) => {
                            warn!("Issue with monitoring actuators: {:?}", e); return },
                        }
                }
                msg = uploader_monitor.recv() => {
                    match msg {
                        Ok(c) => change(self.inner.uploaders.iter(), c),
                        Err(e) => {
                            warn!("Issue with monitoring uploaders: {:?}", e); return },
                    }
                }
            };
            if changed {
                self.inner.notifier.notify().await;
            }
        }
    }
}

struct ServerInner {
    devices: Registry<Device>,
    consoles: Registry<Arc<dyn Console>>,
    actuators: Registry<Arc<dyn Actuator>>,
    uploaders: Registry<Arc<dyn Uploader>>,
}

fn to_item_list<T: Clone>(registry: &Registry<T>) -> ItemList {
    let item = registry
        .contents()
        .into_iter()
        .map(|content| boardswarm_protocol::Item {
            id: content.0,
            name: content.1.name().to_string(),
        })
        .collect();
    ItemList { item }
}

#[derive(Clone)]
pub struct Server {
    inner: Arc<ServerInner>,
}

impl Server {
    fn new() -> Self {
        Self {
            inner: Arc::new(ServerInner {
                consoles: Registry::new(),
                devices: Registry::new(),
                actuators: Registry::new(),
                uploaders: Registry::new(),
            }),
        }
    }

    fn register_actuator<A>(&self, properties: Properties, actuator: A) -> u64
    where
        A: Actuator + 'static,
    {
        let name = properties.name().to_owned();
        let id = self.inner.actuators.add(properties, Arc::new(actuator));
        info!("Registered actuator: {} - {}", id, name);
        id
    }

    fn get_actuator(&self, name: &str) -> Option<Arc<dyn Actuator>> {
        self.inner
            .actuators
            .find_by_name(name)
            .map(|(_, _, actuator)| actuator)
    }

    fn find_actuator<'a, K, V, I>(&self, matches: &'a I) -> Option<Arc<dyn Actuator>>
    where
        K: AsRef<str>,
        V: AsRef<str>,
        &'a I: IntoIterator<Item = (K, V)>,
    {
        self.inner
            .actuators
            .find(matches)
            .map(|(_, _, actuator)| actuator)
    }

    fn register_console<C>(&self, properties: Properties, console: C) -> u64
    where
        C: Console + 'static,
    {
        let name = properties.name().to_owned();
        let id = self.inner.consoles.add(properties, Arc::new(console));
        info!("Registered console: {} - {}", id, name);
        id
    }

    fn unregister_console(&self, id: u64) {
        if let Some((p, _)) = self.inner.consoles.lookup(id) {
            info!("Unregistering console: {} - {}", id, p.name());
            self.inner.consoles.remove(id);
        }
    }

    fn get_console(&self, id: u64) -> Option<Arc<dyn Console>> {
        self.inner.consoles.lookup(id).map(|(_, console)| console)
    }

    fn register_uploader<U>(&self, properties: Properties, uploader: U) -> u64
    where
        U: Uploader + 'static,
    {
        let name = properties.name().to_owned();
        let id = self.inner.uploaders.add(properties, Arc::new(uploader));
        info!("Registered uploader: {} - {}", id, name);
        id
    }

    fn unregister_uploader(&self, id: u64) {
        if let Some((p, _)) = self.inner.uploaders.lookup(id) {
            info!("Unregistering uploader: {} - {}", id, p.name());
            self.inner.uploaders.remove(id);
        }
    }

    pub fn get_uploader(&self, id: u64) -> Option<Arc<dyn Uploader>> {
        self.inner
            .uploaders
            .lookup(id)
            .map(|(_, uploader)| uploader)
    }

    fn register_device(&self, device: Device) {
        let properties = Properties::new(device.name());
        let id = self.inner.devices.add(properties, device.clone());
        info!("Registered device: {} - {}", id, device.name());
    }

    fn get_device(&self, id: u64) -> Option<Device> {
        self.inner.devices.lookup(id).map(|(_, d)| d)
    }

    fn item_list_for(&self, type_: boardswarm_protocol::ItemType) -> ItemList {
        match type_ {
            boardswarm_protocol::ItemType::Actuator => to_item_list(&self.inner.actuators),
            boardswarm_protocol::ItemType::Device => to_item_list(&self.inner.devices),
            boardswarm_protocol::ItemType::Console => to_item_list(&self.inner.consoles),
            boardswarm_protocol::ItemType::Uploader => to_item_list(&self.inner.uploaders),
        }
    }
}

type ItemMonitorStream = BoxStream<'static, Result<boardswarm_protocol::ItemEvent, tonic::Status>>;
#[tonic::async_trait]
impl boardswarm_protocol::boardswarm_server::Boardswarm for Server {
    async fn list(
        &self,
        request: tonic::Request<ItemTypeRequest>,
    ) -> Result<tonic::Response<ItemList>, tonic::Status> {
        let request = request.into_inner();
        let type_ = boardswarm_protocol::ItemType::from_i32(request.r#type)
            .ok_or_else(|| tonic::Status::invalid_argument("Unknown item type "))?;

        Ok(tonic::Response::new(self.item_list_for(type_)))
    }

    type MonitorStream = ItemMonitorStream;
    async fn monitor(
        &self,
        request: tonic::Request<ItemTypeRequest>,
    ) -> Result<tonic::Response<Self::MonitorStream>, tonic::Status> {
        let request = request.into_inner();
        let type_ = boardswarm_protocol::ItemType::from_i32(request.r#type)
            .ok_or_else(|| tonic::Status::invalid_argument("Unknown item type "))?;

        fn to_item_stream<T>(registry: &Registry<T>) -> ItemMonitorStream
        where
            T: Clone + Send + 'static,
        {
            let monitor = registry.monitor();
            let initial = Ok(ItemEvent {
                event: Some(Event::Add(to_item_list(registry))),
            });
            stream::once(async move { initial })
                .chain(stream::unfold(monitor, |mut monitor| async move {
                    let event = monitor.recv().await.ok()?;
                    match event {
                        registry::RegistryChange::Added { id, properties, .. } => Some((
                            Ok(ItemEvent {
                                event: Some(Event::Add(ItemList {
                                    item: vec![boardswarm_protocol::Item {
                                        id,
                                        name: properties.name().to_string(),
                                    }],
                                })),
                            }),
                            monitor,
                        )),
                        registry::RegistryChange::Removed(removed) => Some((
                            Ok(boardswarm_protocol::ItemEvent {
                                event: Some(Event::Remove(removed)),
                            }),
                            monitor,
                        )),
                    }
                }))
                .boxed()
        }
        let response = match type_ {
            boardswarm_protocol::ItemType::Actuator => to_item_stream(&self.inner.actuators),
            boardswarm_protocol::ItemType::Device => to_item_stream(&self.inner.devices),
            boardswarm_protocol::ItemType::Console => to_item_stream(&self.inner.consoles),
            boardswarm_protocol::ItemType::Uploader => to_item_stream(&self.inner.uploaders),
        };
        Ok(tonic::Response::new(response))
    }

    async fn console_configure(
        &self,
        request: tonic::Request<ConsoleConfigureRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.get_console(inner.console) {
            console
                .configure(Box::new(<dyn erased_serde::Deserializer>::erase(
                    inner.parameters.unwrap(),
                )))
                .unwrap();
            Ok(tonic::Response::new(()))
        } else {
            Err(tonic::Status::invalid_argument("Can't find console"))
        }
    }

    type ConsoleStreamOutputStream = ConsoleOutputStream;
    async fn console_stream_output(
        &self,
        request: tonic::Request<ConsoleOutputRequest>,
    ) -> Result<tonic::Response<Self::ConsoleStreamOutputStream>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(console) = self.get_console(inner.console) {
            Ok(tonic::Response::new(console.output_stream().await))
        } else {
            Err(tonic::Status::invalid_argument("Can't find output console"))
        }
    }

    async fn console_stream_input(
        &self,
        request: tonic::Request<Streaming<ConsoleInputRequest>>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let mut rx = request.into_inner();

        /* First message must select the target */
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => return Ok(tonic::Response::new(())),
        };
        let console = if let Some(console_input_request::TargetOrData::Console(console)) =
            msg.target_or_data
        {
            self.get_console(console)
                .ok_or_else(|| tonic::Status::not_found("No serial console by that name"))?
        } else {
            return Err(tonic::Status::invalid_argument(
                "Target should be set first",
            ));
        };

        let mut input = console.input().await.unwrap();
        while let Some(request) = rx.message().await? {
            match request.target_or_data {
                Some(console_input_request::TargetOrData::Data(data)) => {
                    input.send(data).await.unwrap()
                }
                _ => return Err(tonic::Status::invalid_argument("Target cannot be changed")),
            }
        }
        Ok(tonic::Response::new(()))
    }

    type DeviceInfoStream = BoxStream<'static, Result<boardswarm_protocol::Device, tonic::Status>>;
    async fn device_info(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceRequest>,
    ) -> Result<tonic::Response<Self::DeviceInfoStream>, tonic::Status> {
        let request = request.into_inner();
        if let Some((_, device)) = self.inner.devices.lookup(request.device) {
            let info = (&device).into();
            let monitor = device.updates();
            let stream = Box::pin(stream::once(async move { Ok(info) }).chain(stream::unfold(
                (device, monitor),
                |(device, mut monitor)| async move {
                    monitor.wait().await.ok()?;
                    let info = (&device).into();
                    Some((Ok(info), (device, monitor)))
                },
            )));
            Ok(tonic::Response::new(stream))
        } else {
            Err(tonic::Status::not_found("No such device"))
        }
    }

    async fn device_change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::DeviceModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        if let Some(device) = self.get_device(request.device) {
            match device.set_mode(&request.mode).await {
                Ok(()) => Ok(tonic::Response::new(())),
                Err(DeviceSetModeError::ModeNotFound) => {
                    Err(tonic::Status::not_found("No mode by that name"))
                }
                Err(DeviceSetModeError::WrongCurrentMode) => Err(
                    tonic::Status::failed_precondition("Not in the right mode to switch"),
                ),
                Err(DeviceSetModeError::ActuatorFailed(_)) => {
                    Err(tonic::Status::aborted("Actuator failed"))
                }
            }
        } else {
            Err(tonic::Status::not_found("No device by that id"))
        }
    }

    async fn actuator_change_mode(
        &self,
        request: tonic::Request<boardswarm_protocol::ActuatorModeRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let inner = request.into_inner();
        if let Some(actuator) = self.get_actuator(&inner.actuator) {
            actuator
                .set_mode(Box::new(<dyn erased_serde::Deserializer>::erase(
                    inner.parameters.unwrap(),
                )))
                .await
                .unwrap();
            Ok(tonic::Response::new(()))
        } else {
            Err(tonic::Status::invalid_argument("Can't find actuator"))
        }
    }

    type UploaderUploadStream = UploadProgressStream;
    async fn uploader_upload(
        &self,
        request: tonic::Request<tonic::Streaming<boardswarm_protocol::UploadRequest>>,
    ) -> Result<tonic::Response<Self::UploaderUploadStream>, tonic::Status> {
        let mut rx = request.into_inner();
        let msg = match rx.message().await? {
            Some(msg) => msg,
            None => {
                return Err(tonic::Status::invalid_argument(
                    "No uploader/target selection",
                ))
            }
        };

        if let Some(upload_request::TargetOrData::Target(target)) = msg.target_or_data {
            let uploader = self
                .inner
                .uploaders
                .lookup(target.uploader)
                .map(|(_, u)| u)
                .ok_or_else(|| tonic::Status::not_found("No uploader console by that name"))?;

            let data = stream::unfold(rx, |mut rx| async move {
                // TODO handle errors
                if let Some(msg) = rx.message().await.ok()? {
                    match msg.target_or_data {
                        Some(upload_request::TargetOrData::Data(data)) => Some((data, rx)),
                        _ => None, // TODO this is an error!
                    }
                } else {
                    None
                }
            })
            .boxed();

            let (progress, progress_stream) = UploadProgress::new();
            tokio::spawn(async move {
                uploader
                    .upload(&target.target, data, target.length, progress)
                    .await
                    .unwrap()
            });

            Ok(tonic::Response::new(progress_stream))
        } else {
            Err(tonic::Status::invalid_argument(
                "Target should be set first",
            ))
        }
    }

    async fn uploader_commit(
        &self,
        request: tonic::Request<UploaderRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let request = request.into_inner();
        let uploader = self
            .get_uploader(request.uploader)
            .ok_or_else(|| tonic::Status::not_found("Uploader not found"))?;
        uploader
            .commit()
            .await
            .map_err(|_e| tonic::Status::unknown("Commit failed"))?;
        Ok(tonic::Response::new(()))
    }

    async fn uploader_info(
        &self,
        request: tonic::Request<UploaderRequest>,
    ) -> Result<tonic::Response<UploaderInfoMsg>, tonic::Status> {
        let request = request.into_inner();
        let uploader = self
            .get_uploader(request.uploader)
            .ok_or_else(|| tonic::Status::not_found("Uploader not found"))?;

        let info = UploaderInfoMsg {
            target: uploader
                .targets()
                .iter()
                .cloned()
                .map(|name| boardswarm_protocol::UploaderTarget { name })
                .collect(),
        };
        Ok(tonic::Response::new(info))
    }
}

#[derive(Debug, clap::Parser)]
struct Opts {
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let opts = Opts::parse();
    let config = config::Config::from_file(opts.config)?;

    let server = Server::new();
    for d in config.devices {
        let device = Device::from_config(d, server.clone());
        server.register_device(device.clone());
        tokio::spawn(async move {
            loop {
                device.monitor_items().await
            }
        });
    }

    for p in config.providers {
        if p.type_ == "pdudaemon" {
            pdudaemon::start_provider(p.name, p.parameters.unwrap(), server.clone());
        }
    }

    let local = tokio::task::LocalSet::new();
    local.spawn_local(udev::start_provider("udev".to_string(), server.clone()));

    let server = tonic::transport::Server::builder()
        .add_service(boardswarm_protocol::boardswarm_server::BoardswarmServer::new(server.clone()))
        .serve("[::1]:50051".to_socket_addrs().unwrap().next().unwrap());
    info!("Server listening");
    tokio::join!(local, server).1?;

    Ok(())
}
