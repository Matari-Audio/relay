//! RELAY: an insert that shares the track it sits on, or plays someone
//! else's. The audio thread only moves samples through two `rtrb` rings;
//! the link thread in `relay-core` does the networking.
#![forbid(unsafe_code)]

mod ui;

use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, RwLock};

use relay_core::{Link, Playout, Role, Shared};
use truce::prelude::*;
use truce_core::custom_state::{PersistField, StateCursor};

pub(crate) use RelayParamsParamId as P;

#[derive(ParamEnum, Debug)]
pub enum Mode {
    #[name = "Off"]
    Off,
    #[name = "Share"]
    Share,
    #[name = "Join"]
    Join,
}

#[derive(Params)]
pub struct RelayParams {
    #[param(name = "Mode")]
    pub mode: EnumParam<Mode>,
    /// Peak of what is shared (Share) or received (Join), per side.
    #[meter]
    pub left: MeterSlot,
    #[meter]
    pub right: MeterSlot,
    #[persist = "session"]
    pub link: Session,
}

/// The editor's and link's handle on [`Shared`]; saves room, password,
/// direct host and the room's host key with the project.
#[derive(Clone)]
pub struct Session(pub Arc<Shared>);

impl Default for Session {
    fn default() -> Self {
        let shared = Shared::new();
        shared.set_text(&shared.room, &relay_core::room_name());
        shared.set_text(&shared.host_key, &relay_core::random_key());
        Self(shared)
    }
}

#[derive(State, Clone, Debug, Default, PartialEq, Eq)]
struct Saved {
    room: String,
    password: String,
    peer: String,
    host_key: String,
}

impl PersistField for Session {
    fn persist_write(&self, buf: &mut Vec<u8>) {
        let s = &self.0;
        RwLock::new(Saved {
            room: Shared::text(&s.room),
            password: Shared::text(&s.password),
            peer: Shared::text(&s.peer),
            host_key: Shared::text(&s.host_key),
        })
        .persist_write(buf);
    }

    fn persist_read(&self, cursor: &mut StateCursor) {
        let saved = RwLock::new(Saved::default());
        saved.persist_read(cursor);
        let saved = saved.into_inner().unwrap_or_default();
        let s = &self.0;
        if !saved.room.is_empty() {
            s.set_text(&s.room, &saved.room);
        }
        s.set_text(&s.password, &saved.password);
        s.set_text(&s.peer, &saved.peer);
        if !saved.host_key.is_empty() {
            s.set_text(&s.host_key, &saved.host_key);
        }
    }
}

pub struct Relay;

pub struct Dsp {
    shared: Arc<Shared>,
    tx: relay_core::rtrb::Producer<f32>,
    playout: Playout,
    left: Vec<f32>,
    right: Vec<f32>,
    /// Stops and joins the link thread on drop.
    _link: Option<Link>,
}

impl Dsp {
    fn new(shared: Arc<Shared>, spawn: bool) -> Self {
        let ((tx, tx_out), (rx_in, rx)) = relay_core::rings();
        Self {
            _link: spawn.then(|| Link::spawn(Arc::clone(&shared), tx_out, rx_in)),
            shared,
            tx,
            playout: Playout::new(rx),
            left: Vec::new(),
            right: Vec::new(),
        }
    }
}

/// Unlinked: what truce holds before `init`.
impl Default for Dsp {
    fn default() -> Self {
        Self::new(Shared::new(), false)
    }
}

impl PluginLogic for Relay {
    type Params = RelayParams;
    type DspState = Dsp;

    const PRESERVE_DSP_STATE: bool = false;

    fn init(params: &RelayParams, _cx: &InitContext) -> Dsp {
        Dsp::new(Arc::clone(&params.link.0), true)
    }

    fn reset(state: &mut Dsp, _params: &RelayParams, config: &AudioConfig) {
        let rate = config.sample_rate.round().clamp(8_000.0, 384_000.0) as u32;
        state.shared.rate.store(rate, Relaxed);
        state.left.resize(config.max_block_size, 0.0);
        state.right.resize(config.max_block_size, 0.0);
    }

    fn process(
        state: &mut Dsp,
        params: &RelayParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let role = match params.mode.value() {
            Mode::Off => Role::Off,
            Mode::Share => Role::Share,
            Mode::Join => Role::Join,
        };
        state.shared.set_role(role);
        let n = buffer.num_samples().min(state.left.len());
        state.shared.block.store(n as u32, Relaxed);
        let outs = buffer.num_output_channels().min(2);
        for ch in 0..outs {
            let (input, output) = buffer.io(ch);
            if !std::ptr::eq(input.as_ptr(), output.as_ptr()) {
                output[..n].copy_from_slice(&input[..n]);
            }
        }
        let (l, r) = (&mut state.left[..n], &mut state.right[..n]);
        l.copy_from_slice(&buffer.input(0)[..n]);
        r.copy_from_slice(&buffer.input(buffer.num_input_channels().min(2) - 1)[..n]);

        match role {
            Role::Share => {
                if let Ok(chunk) = state.tx.write_chunk_uninit(n * 2) {
                    chunk.fill_from_iter(l.iter().zip(r.iter()).flat_map(|(a, b)| [*a, *b]));
                }
            }
            Role::Join => {
                l.fill(0.0);
                r.fill(0.0);
                state.playout.render(l, r);
                state
                    .shared
                    .latency
                    .store(state.playout.target() as u32, Relaxed);
                for (ch, src) in [&*l, &*r].into_iter().enumerate().take(outs) {
                    for (o, s) in buffer.output(ch)[..n].iter_mut().zip(src) {
                        *o += s;
                    }
                }
            }
            Role::Off => l.fill(0.0),
        }
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, s| m.max(s.abs())).min(1.0);
        context.set_meter(params.left.id(), peak(l));
        context.set_meter(params.right.id(), peak(r));
        ProcessStatus::Normal
    }

    fn editor(params: Arc<RelayParams>) -> Box<dyn Editor> {
        ui::editor(params)
    }
}

truce::plugin! {
    logic: Relay,
    params: RelayParams,
}
