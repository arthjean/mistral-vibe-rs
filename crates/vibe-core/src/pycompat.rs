//! The Python and pydantic behaviors the reference's observable output rests
//! on, each implemented once.
//!
//! The reference quotes names with `repr`, writes fingerprints and headers
//! with `json.dumps`, strips text with `str.strip`, reports a failed read as
//! `str(OSError)` and a refused payload as `str(ValidationError)`. Those
//! spellings reach the model, the user and the files another implementation
//! reads, so every crate goes through this module instead of approximating
//! them locally. Reference behavior is CPython 3.12 and pydantic 2.13 (core
//! 2.46), the versions the pinned checkout's environment runs.

mod json;
mod number;
mod os_error;
pub mod pydantic;
mod repr;
mod text;

pub use json::{Dumps, dumps, python_eq, truthy};
pub use number::{parse_float, parse_int, repr_float, thousands};
pub use os_error::os_error;
pub use repr::{repr_json, repr_str, str_json};
pub use text::{is_line_boundary, isprintable, isspace, splitlines, strip, title};

#[cfg(test)]
mod pycompat_tests;
