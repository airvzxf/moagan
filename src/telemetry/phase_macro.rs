//! D.17.10: `phase!` macro.

#[macro_export]
#[allow(missing_docs)]
macro_rules! phase {
    ($name:expr, $($field:tt)*) => {
        tracing::info_span!(stringify!($name), $($field)*)
    };
    ($name:expr) => {
        tracing::info_span!(stringify!($name))
    };
}

pub use phase;
