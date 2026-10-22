//! PipeWire thread: main loop, the two capture streams, registry and the
//! `default` metadata (spec 5.2, 5.3, 6.5).
//!
//! Device selection is left to the session manager: streams are connected
//! without a target, so WirePlumber links them to the current default devices
//! and moves them when the default changes. This module only observes which
//! device each stream ends up linked to, and re-connects a stream (still
//! without a target) if WirePlumber did not move it in time.
//!
//! PipeWire objects are not `Send`; all of them live in the thread started by
//! [`spawn`]. Other threads receive PCM and events through crossbeam channels
//! and send commands through `pipewire::channel`.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Cursor};
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, Listener as CoreListener};
use pipewire::keys;
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::{Metadata, MetadataListener};
use pipewire::properties::properties;
use pipewire::registry::{Listener as RegistryListener, RegistryRc};
use pipewire::spa::param::ParamType;
use pipewire::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Value};
use pipewire::spa::utils::{Direction, SpaTypes};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::types::ObjectType;

use crate::mixer::{Channel, SAMPLE_RATE};

/// Samples in one [`PcmChunk`]. Larger PipeWire buffers are split.
pub const CHUNK_SAMPLES: usize = 2048;
/// Capacity of the PCM channel in chunks (≥ 4 s of audio per channel at
/// typical buffer sizes). On overflow the realtime callback drops data.
pub const PCM_QUEUE_CHUNKS: usize = 128;
/// How often the PipeWire thread checks the graph and reports changes.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(250);
/// Pause between attempts to (re)connect to PipeWire (spec 6.5).
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
/// How long WirePlumber gets to move a stream after a default change before
/// the stream is re-connected by hand (spec 5.3).
const RECONNECT_GRACE: Duration = Duration::from_secs(2);
/// `default` metadata keys (spec 5.3).
const DEFAULT_SOURCE_KEY: &str = "default.audio.source";
const DEFAULT_SINK_KEY: &str = "default.audio.sink";
/// `SPA_ID_INVALID`: node id of a stream that has no node yet.
const INVALID_ID: u32 = u32::MAX;

/// Captured mono samples of one channel, copied out of a PipeWire buffer.
/// Fixed-size so the realtime callback never allocates.
#[derive(Clone)]
pub struct PcmChunk {
    pub channel: Channel,
    len: usize,
    samples: [i16; CHUNK_SAMPLES],
}

impl PcmChunk {
    #[must_use]
    pub fn samples(&self) -> &[i16] {
        self.samples.get(..self.len).unwrap_or(&[])
    }
}

impl std::fmt::Debug for PcmChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PcmChunk")
            .field("channel", &self.channel)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// A PipeWire node: system name and human-readable description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub name: String,
    pub description: String,
}

/// What the PipeWire thread reports. Texts are for the UI event log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PwEvent {
    /// The system default device of a channel changed (`None`: no default).
    DefaultChanged {
        channel: Channel,
        device: Option<Device>,
    },
    /// The capture stream of a channel is linked to `device` now (`None`: not
    /// linked; the channel records silence).
    StreamDevice {
        channel: Channel,
        device: Option<Device>,
    },
    /// The safety net re-connected a stream that WirePlumber did not move.
    ManualReconnect { channel: Channel },
    /// Connected to PipeWire (again).
    Connected,
    /// Connection lost or not possible; retried every second.
    Disconnected { reason: String },
    /// A non-fatal error.
    Error(String),
    /// The PipeWire thread could not start and exited.
    Fatal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PwError {
    #[error("PipeWire: {0}")]
    PipeWire(#[from] pipewire::Error),
    #[error("формат потока: {0}")]
    Format(String),
    #[error("таймер PipeWire: {0}")]
    Timer(String),
}

enum Command {
    Quit,
}

/// Handle of the PipeWire thread. Dropping it stops the thread.
pub struct PwHandle {
    commands: pipewire::channel::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PwHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PwHandle").finish_non_exhaustive()
    }
}

impl PwHandle {
    /// Stops the main loop and waits for the thread to exit.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        // Fails only if the thread is gone already; joining is still right.
        let _ = self.commands.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PwHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the PipeWire thread. PCM goes to `pcm` (use a bounded channel of
/// [`PCM_QUEUE_CHUNKS`]), everything else to `events`.
pub fn spawn(pcm: Sender<PcmChunk>, events: Sender<PwEvent>) -> io::Result<PwHandle> {
    let (commands, receiver) = pipewire::channel::channel();
    let thread = thread::Builder::new()
        .name("pipewire".into())
        .spawn(move || {
            if let Err(e) = run(receiver, &pcm, &events) {
                let _ = events.send(PwEvent::Fatal(e.to_string()));
            }
        })?;
    Ok(PwHandle {
        commands,
        thread: Some(thread),
    })
}

fn run(
    commands: pipewire::channel::Receiver<Command>,
    pcm: &Sender<PcmChunk>,
    events: &Sender<PwEvent>,
) -> Result<(), PwError> {
    pipewire::init();
    let main_loop = MainLoopRc::new(None)?;
    let runtime = Rc::new(RefCell::new(Runtime::new(pcm.clone(), events.clone())));

    let weak_loop = main_loop.downgrade();
    let _commands = commands.attach(main_loop.loop_(), move |command| match command {
        Command::Quit => {
            if let Some(main_loop) = weak_loop.upgrade() {
                main_loop.quit();
            }
        }
    });

    let timer_loop = main_loop.clone();
    let timer_runtime = Rc::clone(&runtime);
    let timer = main_loop.loop_().add_timer(move |_| {
        if let Ok(mut runtime) = timer_runtime.try_borrow_mut() {
            runtime.housekeeping(&timer_loop, Instant::now());
        }
    });
    timer
        .update_timer(Some(Duration::from_millis(1)), Some(HOUSEKEEPING_INTERVAL))
        .into_result()
        .map_err(|e| PwError::Timer(e.to_string()))?;

    main_loop.run();
    // Streams and the connection go before the loop they are attached to.
    drop(timer);
    if let Ok(mut runtime) = runtime.try_borrow_mut() {
        runtime.session = None;
    }
    Ok(())
}

/// Connection state of the PipeWire thread, driven by the housekeeping timer.
struct Runtime {
    session: Option<Session>,
    next_attempt: Instant,
    /// Last reported connection state; `None` before the first attempt.
    online: Option<bool>,
    pcm: Sender<PcmChunk>,
    events: Sender<PwEvent>,
}

impl Runtime {
    fn new(pcm: Sender<PcmChunk>, events: Sender<PwEvent>) -> Self {
        Self {
            session: None,
            next_attempt: Instant::now(),
            online: None,
            pcm,
            events,
        }
    }

    fn housekeeping(&mut self, main_loop: &MainLoopRc, now: Instant) {
        if let Some(reason) = self.session.as_ref().and_then(Session::lost_reason) {
            // Dropped from the timer, outside of any callback of the session.
            self.session = None;
            self.next_attempt = now + RETRY_INTERVAL;
            self.set_offline(reason);
        }

        if self.session.is_none() && now >= self.next_attempt {
            match Session::connect(main_loop, &self.pcm, &self.events) {
                Ok(session) => {
                    self.session = Some(session);
                    if self.online != Some(true) {
                        self.online = Some(true);
                        self.send(PwEvent::Connected);
                    }
                }
                Err(e) => {
                    self.next_attempt = now + RETRY_INTERVAL;
                    self.set_offline(e.to_string());
                }
            }
        }

        if let Some(session) = &self.session {
            session.refresh(now, &self.events);
        }
    }

    fn set_offline(&mut self, reason: String) {
        if self.online == Some(false) {
            return;
        }
        self.online = Some(false);
        self.send(PwEvent::Disconnected { reason });
        for channel in [Channel::Mic, Channel::Them] {
            self.send(PwEvent::StreamDevice {
                channel,
                device: None,
            });
        }
    }

    fn send(&self, event: PwEvent) {
        // The receiver is gone only while the application shuts down.
        let _ = self.events.send(event);
    }
}

/// One connection to PipeWire with its streams. Recreated after a loss.
/// Fields drop in declaration order: streams first, the context last.
struct Session {
    mic: Capture,
    them: Capture,
    _metadata: Rc<RefCell<HashMap<u32, (Metadata, MetadataListener)>>>,
    _registry_listener: RegistryListener,
    _registry: RegistryRc,
    _core_listener: CoreListener,
    _core: CoreRc,
    _context: ContextRc,
    graph: Rc<RefCell<Graph>>,
    lost: Rc<Cell<bool>>,
    lost_reason: Rc<RefCell<String>>,
}

impl Session {
    fn connect(
        main_loop: &MainLoopRc,
        pcm: &Sender<PcmChunk>,
        events: &Sender<PwEvent>,
    ) -> Result<Self, PwError> {
        let context = ContextRc::new(main_loop, None)?;
        let core = context.connect_rc(None)?;
        let graph = Rc::new(RefCell::new(Graph::default()));
        let lost = Rc::new(Cell::new(false));
        let lost_reason = Rc::new(RefCell::new(String::new()));

        let core_listener = {
            let (lost, lost_reason, events) =
                (Rc::clone(&lost), Rc::clone(&lost_reason), events.clone());
            core.add_listener_local()
                .error(move |id, _seq, res, message| {
                    if id == pipewire::core::PW_ID_CORE {
                        // The connection is unusable; housekeeping replaces it.
                        lost.set(true);
                        if let Ok(mut reason) = lost_reason.try_borrow_mut() {
                            *reason = format!("{message} ({res})");
                        }
                    } else {
                        let _ =
                            events.send(PwEvent::Error(format!("объект {id}: {message} ({res})")));
                    }
                })
                .register()
        };

        let registry = core.get_registry_rc()?;
        let metadata = Rc::new(RefCell::new(HashMap::new()));
        let registry_listener = {
            let (graph_add, graph_remove) = (Rc::clone(&graph), Rc::clone(&graph));
            let (metadata_add, metadata_remove) = (Rc::clone(&metadata), Rc::clone(&metadata));
            let weak_registry = registry.downgrade();
            let events = events.clone();
            registry
                .add_listener_local()
                .global(move |global| {
                    let Some(props) = global.props else { return };
                    match global.type_ {
                        ObjectType::Node => {
                            if let Some(name) = props.get(*keys::NODE_NAME) {
                                let description = props
                                    .get(*keys::NODE_DESCRIPTION)
                                    .or_else(|| props.get(*keys::NODE_NICK))
                                    .unwrap_or(name);
                                with_graph(&graph_add, |g| {
                                    g.add_node(global.id, name, description);
                                });
                            }
                        }
                        ObjectType::Link => {
                            let node = |key| props.get(key).and_then(|v| v.parse::<u32>().ok());
                            if let (Some(output), Some(input)) =
                                (node(*keys::LINK_OUTPUT_NODE), node(*keys::LINK_INPUT_NODE))
                            {
                                with_graph(&graph_add, |g| g.add_link(global.id, output, input));
                            }
                        }
                        ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                            let Some(registry) = weak_registry.upgrade() else {
                                return;
                            };
                            match registry.bind::<Metadata, _>(global) {
                                Ok(proxy) => {
                                    let listener = watch_defaults(&proxy, Rc::clone(&graph_add));
                                    if let Ok(mut store) = metadata_add.try_borrow_mut() {
                                        store.insert(global.id, (proxy, listener));
                                    }
                                    with_graph(&graph_add, |g| g.set_metadata(global.id));
                                }
                                Err(e) => {
                                    let _ = events
                                        .send(PwEvent::Error(format!("метаданные default: {e}")));
                                }
                            }
                        }
                        _ => {}
                    }
                })
                .global_remove(move |id| {
                    with_graph(&graph_remove, |g| g.remove(id, Instant::now()));
                    if let Ok(mut store) = metadata_remove.try_borrow_mut() {
                        store.remove(&id);
                    }
                })
                .register()
        };

        let mic = Capture::new(&core, Channel::Mic, pcm, events)?;
        let them = Capture::new(&core, Channel::Them, pcm, events)?;

        Ok(Self {
            mic,
            them,
            _metadata: metadata,
            _registry_listener: registry_listener,
            _registry: registry,
            _core_listener: core_listener,
            _core: core,
            _context: context,
            graph,
            lost,
            lost_reason,
        })
    }

    fn lost_reason(&self) -> Option<String> {
        self.lost.get().then(|| {
            self.lost_reason
                .try_borrow()
                .map(|r| r.clone())
                .unwrap_or_default()
        })
    }

    /// Reads stream nodes, reports graph changes and runs the safety net.
    fn refresh(&self, now: Instant, events: &Sender<PwEvent>) {
        let (changes, reconnects) = {
            let Ok(mut graph) = self.graph.try_borrow_mut() else {
                return;
            };
            for capture in [&self.mic, &self.them] {
                graph.set_stream_node(capture.channel, capture.node_id());
            }
            (graph.changes(now), graph.due_reconnects(now))
        };
        for event in changes {
            let _ = events.send(event);
        }
        for channel in reconnects {
            let capture = match channel {
                Channel::Mic => &self.mic,
                Channel::Them => &self.them,
            };
            let event = match capture.reconnect() {
                Ok(()) => PwEvent::ManualReconnect { channel },
                Err(e) => PwEvent::Error(format!("переподключение: {e}")),
            };
            let _ = events.send(event);
        }
    }
}

/// Runs `f` on the graph. Callbacks never nest while the graph is borrowed,
/// so a failed borrow is a bug; the next housekeeping pass still reports a
/// consistent state.
fn with_graph(graph: &RefCell<Graph>, f: impl FnOnce(&mut Graph)) {
    if let Ok(mut graph) = graph.try_borrow_mut() {
        f(&mut graph);
    }
}

fn watch_defaults(metadata: &Metadata, graph: Rc<RefCell<Graph>>) -> MetadataListener {
    metadata
        .add_listener_local()
        .property(move |subject, key, _type, value| {
            if subject != pipewire::core::PW_ID_CORE {
                return 0;
            }
            let now = Instant::now();
            let name = value.and_then(parse_default);
            with_graph(&graph, |g| match key {
                // A missing key clears all properties.
                None => {
                    g.set_default(Channel::Mic, None, now);
                    g.set_default(Channel::Them, None, now);
                }
                Some(DEFAULT_SOURCE_KEY) => g.set_default(Channel::Mic, name, now),
                Some(DEFAULT_SINK_KEY) => g.set_default(Channel::Them, name, now),
                Some(_) => {}
            });
            0
        })
        .register()
}

/// `{"name":"bluez_input.70_BF_92_00_00_00.0"}` → the name.
fn parse_default(value: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

/// A capture stream of one channel (spec 5.2).
struct Capture {
    channel: Channel,
    stream: StreamRc,
    _listener: StreamListener<()>,
    format: Vec<u8>,
}

impl Capture {
    fn new(
        core: &CoreRc,
        channel: Channel,
        pcm: &Sender<PcmChunk>,
        events: &Sender<PwEvent>,
    ) -> Result<Self, PwError> {
        let (name, description) = match channel {
            Channel::Mic => ("callrec-mic", "callrec: микрофон"),
            Channel::Them => ("callrec-them", "callrec: собеседники"),
        };
        // Never set target.object or node.dont-reconnect: the session manager
        // must stay free to move the stream to the new default (spec 5.2).
        let mut props = properties! {
            *keys::MEDIA_TYPE => "Audio",
            *keys::MEDIA_CATEGORY => "Capture",
            *keys::MEDIA_ROLE => "Communication",
            *keys::NODE_NAME => name,
            *keys::NODE_DESCRIPTION => description,
        };
        if channel == Channel::Them {
            props.insert(*keys::STREAM_CAPTURE_SINK, "true");
        }
        let stream = StreamRc::new(core.clone(), name, props)?;

        let pcm = pcm.clone();
        let events = events.clone();
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, (), _old, new| {
                if let StreamState::Error(message) = new {
                    let _ = events.send(PwEvent::Error(format!("поток {name}: {message}")));
                }
            })
            .process(move |stream, ()| process(stream, channel, &pcm))
            .register()?;

        let capture = Self {
            channel,
            stream,
            _listener: listener,
            format: format_param()?,
        };
        capture.connect()?;
        Ok(capture)
    }

    fn connect(&self) -> Result<(), PwError> {
        let pod = Pod::from_bytes(&self.format)
            .ok_or_else(|| PwError::Format("некорректный POD".to_owned()))?;
        self.stream.connect(
            Direction::Input,
            None,
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
            &mut [pod],
        )?;
        Ok(())
    }

    /// Disconnects and connects again, still without a target (spec 5.3).
    fn reconnect(&self) -> Result<(), PwError> {
        self.stream.disconnect()?;
        self.connect()
    }

    /// Node id of the stream once it is connected.
    fn node_id(&self) -> Option<u32> {
        let ready = matches!(
            self.stream.state(),
            StreamState::Paused | StreamState::Streaming
        );
        let id = self.stream.node_id();
        (ready && id != INVALID_ID).then_some(id)
    }
}

/// Realtime callback: copies samples into fixed-size chunks and hands them
/// over without blocking. No allocation, no locks, no logging here.
fn process(stream: &Stream, channel: Channel, pcm: &Sender<PcmChunk>) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        return;
    };
    let offset = usize::try_from(data.chunk().offset()).unwrap_or(0);
    let size = usize::try_from(data.chunk().size()).unwrap_or(0);
    let Some(bytes) = data
        .data()
        .and_then(|bytes| bytes.get(offset..offset.saturating_add(size)))
    else {
        return;
    };
    for piece in bytes.as_chunks::<2>().0.chunks(CHUNK_SAMPLES) {
        let mut chunk = PcmChunk {
            channel,
            len: piece.len(),
            samples: [0; CHUNK_SAMPLES],
        };
        for (sample, le) in chunk.samples.iter_mut().zip(piece) {
            *sample = i16::from_le_bytes(*le);
        }
        // A full queue means the writer thread is behind. Dropping is the only
        // option here; the mixer pads the gap with silence.
        let _ = pcm.try_send(chunk);
    }
}

/// `EnumFormat` param: S16LE, mono, 16 kHz. The PipeWire adapter converts
/// whatever the device delivers (spec 5.2).
fn format_param() -> Result<Vec<u8>, PwError> {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S16LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);
    let mut position = info.position();
    if let Some(first) = position.first_mut() {
        *first = pipewire::spa::sys::SPA_AUDIO_CHANNEL_MONO;
    }
    info.set_position(position);
    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let (cursor, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| PwError::Format(format!("{e:?}")))?;
    Ok(cursor.into_inner())
}

/// A value per channel.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PerChannel<T> {
    mic: T,
    them: T,
}

impl<T> PerChannel<T> {
    fn get(&self, channel: Channel) -> &T {
        match channel {
            Channel::Mic => &self.mic,
            Channel::Them => &self.them,
        }
    }

    fn get_mut(&mut self, channel: Channel) -> &mut T {
        match channel {
            Channel::Mic => &mut self.mic,
            Channel::Them => &mut self.them,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    output: u32,
    input: u32,
}

/// What the registry and the `default` metadata say about the graph. Pure
/// state, no PipeWire objects, so it is unit-tested.
#[derive(Debug, Default)]
struct Graph {
    nodes: HashMap<u32, Device>,
    /// Ordered by id: a higher id is a newer link.
    links: BTreeMap<u32, Link>,
    metadata: Option<u32>,
    streams: PerChannel<Option<u32>>,
    defaults: PerChannel<Option<String>>,
    /// When the default last changed and whether the safety net already
    /// checked that change.
    default_since: PerChannel<Option<Instant>>,
    reconnect_checked: PerChannel<bool>,
    /// First `changes` call; until something is reported, the graph is still
    /// being filled in.
    started: Option<Instant>,
    reported: bool,
    reported_defaults: PerChannel<Option<String>>,
    reported_devices: PerChannel<Option<Device>>,
}

const CHANNELS: [Channel; 2] = [Channel::Mic, Channel::Them];

impl Graph {
    fn add_node(&mut self, id: u32, name: &str, description: &str) {
        self.nodes.insert(
            id,
            Device {
                name: name.to_owned(),
                description: description.to_owned(),
            },
        );
    }

    fn add_link(&mut self, id: u32, output: u32, input: u32) {
        self.links.insert(id, Link { output, input });
    }

    fn set_metadata(&mut self, id: u32) {
        self.metadata = Some(id);
    }

    fn remove(&mut self, id: u32, now: Instant) {
        self.nodes.remove(&id);
        self.links.remove(&id);
        if self.metadata == Some(id) {
            // The session manager went away; its defaults are stale.
            self.metadata = None;
            for channel in CHANNELS {
                self.set_default(channel, None, now);
            }
        }
    }

    fn set_stream_node(&mut self, channel: Channel, node: Option<u32>) {
        *self.streams.get_mut(channel) = node;
    }

    fn set_default(&mut self, channel: Channel, name: Option<String>, now: Instant) {
        if *self.defaults.get(channel) == name {
            return;
        }
        *self.defaults.get_mut(channel) = name;
        *self.default_since.get_mut(channel) = Some(now);
        *self.reconnect_checked.get_mut(channel) = false;
    }

    /// The device the stream of `channel` is linked to: the output node of
    /// the newest link into the stream node.
    fn stream_device(&self, channel: Channel) -> Option<Device> {
        let stream = (*self.streams.get(channel))?;
        self.links
            .values()
            .rev()
            .find(|link| link.input == stream)
            .and_then(|link| self.nodes.get(&link.output))
            .cloned()
    }

    fn default_device(&self, channel: Channel) -> Option<Device> {
        let name = self.defaults.get(channel).as_ref()?;
        let known = self.nodes.values().find(|node| &node.name == name).cloned();
        Some(known.unwrap_or_else(|| Device {
            name: name.clone(),
            description: name.clone(),
        }))
    }

    /// Events for everything that changed since the previous call. The first
    /// report waits for the `default` metadata (or `RECONNECT_GRACE` without
    /// a session manager), so a fresh connection does not flash
    /// "no device" before the registry is read.
    fn changes(&mut self, now: Instant) -> Vec<PwEvent> {
        let started = *self.started.get_or_insert(now);
        if !self.reported
            && self.metadata.is_none()
            && now.saturating_duration_since(started) < RECONNECT_GRACE
        {
            return Vec::new();
        }
        let first = !self.reported;
        self.reported = true;

        let mut events = Vec::new();
        for channel in CHANNELS {
            if first || self.reported_defaults.get(channel) != self.defaults.get(channel) {
                events.push(PwEvent::DefaultChanged {
                    channel,
                    device: self.default_device(channel),
                });
                self.reported_defaults
                    .get_mut(channel)
                    .clone_from(self.defaults.get(channel));
            }
            let device = self.stream_device(channel);
            if first || *self.reported_devices.get(channel) != device {
                events.push(PwEvent::StreamDevice {
                    channel,
                    device: device.clone(),
                });
                *self.reported_devices.get_mut(channel) = device;
            }
        }
        events
    }

    /// Channels whose stream is still not on the default device
    /// `RECONNECT_GRACE` after the default changed. Each change is checked
    /// once, so a device that cannot be linked does not cause a loop.
    fn due_reconnects(&mut self, now: Instant) -> Vec<Channel> {
        let mut due = Vec::new();
        for channel in CHANNELS {
            let Some(since) = *self.default_since.get(channel) else {
                continue;
            };
            if *self.reconnect_checked.get(channel)
                || now.saturating_duration_since(since) < RECONNECT_GRACE
            {
                continue;
            }
            *self.reconnect_checked.get_mut(channel) = true;
            let Some(default) = self.defaults.get(channel) else {
                continue;
            };
            let linked = self.stream_device(channel).map(|device| device.name);
            if linked.as_ref() != Some(default) {
                due.push(channel);
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIC_STREAM: u32 = 100;
    const THEM_STREAM: u32 = 101;

    #[allow(clippy::unnecessary_wraps)]
    fn device(name: &str, description: &str) -> Option<Device> {
        Some(Device {
            name: name.to_owned(),
            description: description.to_owned(),
        })
    }

    /// Built-in mic (40) and speakers (41), both streams linked to them.
    fn graph(now: Instant) -> Graph {
        let mut g = Graph::default();
        g.add_node(40, "alsa_input.pci", "Встроенный микрофон");
        g.add_node(41, "alsa_output.pci", "Встроенные динамики");
        g.add_node(MIC_STREAM, "callrec-mic", "callrec: микрофон");
        g.add_node(THEM_STREAM, "callrec-them", "callrec: собеседники");
        g.set_stream_node(Channel::Mic, Some(MIC_STREAM));
        g.set_stream_node(Channel::Them, Some(THEM_STREAM));
        g.set_metadata(7);
        g.set_default(Channel::Mic, Some("alsa_input.pci".to_owned()), now);
        g.set_default(Channel::Them, Some("alsa_output.pci".to_owned()), now);
        g.add_link(200, 40, MIC_STREAM);
        // A stereo monitor gives one link per port.
        g.add_link(201, 41, THEM_STREAM);
        g.add_link(202, 41, THEM_STREAM);
        g
    }

    #[test]
    fn parses_default_metadata_value() {
        assert_eq!(
            parse_default(r#"{"name":"bluez_input.70_BF_92_00_00_00.0"}"#),
            Some("bluez_input.70_BF_92_00_00_00.0".to_owned())
        );
        assert_eq!(parse_default("{}"), None);
        assert_eq!(parse_default("not json"), None);
    }

    #[test]
    fn reports_initial_state_once() {
        let now = Instant::now();
        let mut g = graph(now);
        assert_eq!(
            g.changes(now),
            vec![
                PwEvent::DefaultChanged {
                    channel: Channel::Mic,
                    device: device("alsa_input.pci", "Встроенный микрофон"),
                },
                PwEvent::StreamDevice {
                    channel: Channel::Mic,
                    device: device("alsa_input.pci", "Встроенный микрофон"),
                },
                PwEvent::DefaultChanged {
                    channel: Channel::Them,
                    device: device("alsa_output.pci", "Встроенные динамики"),
                },
                PwEvent::StreamDevice {
                    channel: Channel::Them,
                    device: device("alsa_output.pci", "Встроенные динамики"),
                },
            ]
        );
        assert_eq!(g.changes(now), []);
    }

    #[test]
    fn follows_device_switch() {
        let now = Instant::now();
        let mut g = graph(now);
        g.changes(now);

        // USB mic unplugged, headset becomes the default, WirePlumber moves
        // the stream: old link removed, new link created.
        g.add_node(50, "bluez_input.70", "Jabra Evolve2 65 Mono");
        g.set_default(Channel::Mic, Some("bluez_input.70".to_owned()), now);
        g.remove(200, now);
        assert_eq!(
            g.changes(now),
            vec![
                PwEvent::DefaultChanged {
                    channel: Channel::Mic,
                    device: device("bluez_input.70", "Jabra Evolve2 65 Mono"),
                },
                PwEvent::StreamDevice {
                    channel: Channel::Mic,
                    device: None,
                },
            ]
        );
        g.add_link(210, 50, MIC_STREAM);
        assert_eq!(
            g.changes(now),
            vec![PwEvent::StreamDevice {
                channel: Channel::Mic,
                device: device("bluez_input.70", "Jabra Evolve2 65 Mono"),
            }]
        );
        assert_eq!(g.due_reconnects(now + RECONNECT_GRACE), []);
    }

    #[test]
    fn newest_link_wins_during_switch() {
        let now = Instant::now();
        let mut g = graph(now);
        g.add_node(42, "hdmi_output", "HDMI");
        g.add_link(220, 42, THEM_STREAM);
        assert_eq!(
            g.stream_device(Channel::Them),
            device("hdmi_output", "HDMI")
        );
    }

    #[test]
    fn unknown_default_uses_name_as_description() {
        let now = Instant::now();
        let mut g = Graph::default();
        g.set_default(Channel::Mic, Some("usb_mic".to_owned()), now);
        assert_eq!(g.default_device(Channel::Mic), device("usb_mic", "usb_mic"));
    }

    #[test]
    fn safety_net_reconnects_stuck_stream_once() {
        let now = Instant::now();
        let mut g = graph(now);
        assert_eq!(g.due_reconnects(now + RECONNECT_GRACE), []);

        let later = now + Duration::from_secs(10);
        g.add_node(50, "bluez_input.70", "Jabra");
        g.set_default(Channel::Mic, Some("bluez_input.70".to_owned()), later);
        // Still linked to the old device.
        assert_eq!(g.due_reconnects(later + Duration::from_millis(1999)), []);
        assert_eq!(
            g.due_reconnects(later + RECONNECT_GRACE),
            vec![Channel::Mic]
        );
        assert_eq!(g.due_reconnects(later + RECONNECT_GRACE * 2), []);
    }

    #[test]
    fn safety_net_handles_unlinked_stream() {
        let now = Instant::now();
        let mut g = graph(now);
        g.remove(201, now);
        g.remove(202, now);
        assert_eq!(g.due_reconnects(now + RECONNECT_GRACE), vec![Channel::Them]);
    }

    #[test]
    fn no_default_means_nothing_to_reconnect() {
        let now = Instant::now();
        let mut g = graph(now);
        g.remove(200, now);
        g.set_default(Channel::Mic, None, now);
        assert_eq!(g.due_reconnects(now + RECONNECT_GRACE), []);
        assert_eq!(g.default_device(Channel::Mic), None);
    }

    #[test]
    fn removed_metadata_clears_defaults() {
        let now = Instant::now();
        let mut g = graph(now);
        g.set_metadata(7);
        g.changes(now);
        g.remove(7, now);
        assert_eq!(
            g.changes(now),
            vec![
                PwEvent::DefaultChanged {
                    channel: Channel::Mic,
                    device: None,
                },
                PwEvent::DefaultChanged {
                    channel: Channel::Them,
                    device: None,
                },
            ]
        );
    }

    #[test]
    fn stream_without_node_has_no_device() {
        let now = Instant::now();
        let mut g = graph(now);
        g.set_stream_node(Channel::Mic, None);
        assert_eq!(g.stream_device(Channel::Mic), None);
    }

    #[test]
    fn format_param_serializes() {
        let bytes = format_param().unwrap();
        assert!(Pod::from_bytes(&bytes).is_some());
    }
}
