//! A process that has not loaded the library, and is pointed at a place
//! where it is not. Kept apart from the other tests, which load it: a
//! process loads the library once.

use std::path::Path;

use waveport_models_onnx::{LIBRARY, Runtime};
use waveport_ports::error::RetryClass;

#[test]
fn a_library_that_is_not_there_says_how_to_get_it() {
    let nowhere = Path::new(env!("CARGO_MANIFEST_DIR")).join("no-such-directory");
    let error = Runtime::load_from(&nowhere).unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent);
    assert!(error.message.contains(LIBRARY), "{error}");
    assert!(error.message.contains("fetch-assets"), "{error}");

    // A file that is there and is not the library.
    let not_it = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let error = Runtime::load(&not_it).unwrap_err();
    assert_eq!(error.class, RetryClass::Permanent);
    assert!(error.message.contains("could not be loaded"), "{error}");
}
