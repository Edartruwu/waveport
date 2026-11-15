//! The models that listen, run by ONNX Runtime.
//!
//! Stability: experimental
//!
//! Two small models run on this machine, beside the sessions, and never
//! leave it:
//!
//! - [`SileroVad`] says, 31 times a second, how likely it is that a
//!   frame of audio holds speech. It is Silero VAD, version 6.2.3.
//! - [`SmartTurn`] says how likely it is that a speaker who has paused
//!   has finished, from the sound of the last eight seconds. It is Smart
//!   Turn, version 3.2.
//!
//! Both are ONNX models, and ONNX Runtime runs them. The library is not
//! linked into the program: it is a file, loaded once when the program
//! starts ([`Runtime::load`]), and so are the models. `cargo xtask
//! fetch-assets` fetches all three into `target/assets/`, each checked
//! against a digest that the repository pins.
//!
//! Inference runs on the thread that asks for it, and on no other: the
//! library is told to keep one thread and not to spin while it waits.
//! Which thread that is, and how the frames of many sessions are put
//! into one call, is the business of the runtime's compute plane.

mod mel;
mod silero;
mod smart_turn;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use ort::environment::Environment;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use waveport_ports::error::PortError;

pub use crate::silero::{SileroState, SileroVad};
pub use crate::smart_turn::SmartTurn;

/// The name the library's file has on this platform.
pub const LIBRARY: &str = if cfg!(windows) {
    "onnxruntime.dll"
} else if cfg!(target_os = "macos") {
    "libonnxruntime.dylib"
} else {
    "libonnxruntime.so"
};

/// The name of the file of the voice-activity model.
pub const VAD_MODEL: &str = "silero-vad-v6.2.3.onnx";

/// The name of the file of the end-of-turn model.
pub const TURN_MODEL: &str = "smart-turn-v3.2-cpu.onnx";

/// The library, once it is loaded: where from, and its environment.
struct Loaded {
    library: PathBuf,
    environment: Environment,
}

/// A process has one copy of the library, or none.
static LOADED: OnceLock<Loaded> = OnceLock::new();

/// Held while the library is being loaded, so that two threads do not
/// both try.
static LOADING: Mutex<()> = Mutex::new(());

/// What a failure of the library is as a failure of a port. Nothing the
/// library fails at gets better by asking again.
fn failed(what: &str, error: &dyn core::fmt::Display) -> PortError {
    PortError::permanent(format!("{what}: {error}"))
}

/// ONNX Runtime, loaded.
///
/// A process loads the library once. Loading it again from the same
/// file gives the same thing; from another file it is refused.
#[derive(Clone, Copy, Debug)]
pub struct Runtime {
    loaded: &'static Loaded,
}

impl core::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Loaded")
            .field("library", &self.library)
            .finish_non_exhaustive()
    }
}

impl Runtime {
    /// Loads the library from its file.
    ///
    /// # Errors
    ///
    /// The file is not there or is not the library, or the process has
    /// already loaded the library from another file.
    pub fn load(library: &Path) -> Result<Self, PortError> {
        let _one_at_a_time = LOADING.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(loaded) = LOADED.get() {
            if loaded.library == library {
                return Ok(Self { loaded });
            }
            return Err(PortError::permanent(format!(
                "ONNX Runtime was already loaded from {}, and cannot be loaded from {} as well",
                loaded.library.display(),
                library.display()
            )));
        }
        // Said here, with the name of the file: the library's own words
        // for a file that is not there are harder to read.
        if !library.is_file() {
            return Err(PortError::permanent(format!(
                "ONNX Runtime is not at {}; `cargo xtask fetch-assets` fetches it",
                library.display()
            )));
        }
        let environment = ort::init_from(library)
            .map_err(|error| failed("ONNX Runtime could not be loaded", &error))?
            .with_name("waveport")
            .build()
            .map_err(|error| failed("ONNX Runtime could not be started", &error))?;
        let loaded = LOADED.get_or_init(|| Loaded {
            library: library.to_owned(),
            environment,
        });
        Ok(Self { loaded })
    }

    /// Loads the library from the directory `cargo xtask fetch-assets`
    /// fills, or any other that holds it under the name it has on this
    /// platform ([`LIBRARY`]).
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load).
    pub fn load_from(assets: &Path) -> Result<Self, PortError> {
        Self::load(&assets.join(LIBRARY))
    }

    /// The file the library was loaded from.
    #[must_use]
    pub fn library(self) -> &'static Path {
        &self.loaded.library
    }

    /// Loads a model, to be run on the thread that asks and on no other.
    pub(crate) fn session(self, model: &Path) -> Result<Session, PortError> {
        if !model.is_file() {
            return Err(PortError::permanent(format!(
                "the model is not at {}; `cargo xtask fetch-assets` fetches it",
                model.display()
            )));
        }
        let what = "the model could not be set up";
        Session::builder(&self.loaded.environment)
            .map_err(|error| failed(what, &error))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|error| failed(what, &error))?
            .with_intra_threads(1)
            .map_err(|error| failed(what, &error))?
            .with_inter_threads(1)
            .map_err(|error| failed(what, &error))?
            .with_intra_op_spinning(false)
            .map_err(|error| failed(what, &error))?
            .commit_from_file(model)
            .map_err(|error| failed(&format!("{} could not be loaded", model.display()), &error))
    }
}
