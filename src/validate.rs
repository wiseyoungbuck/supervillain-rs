/// Validate a condition at API boundaries. In debug builds, also panics for early detection.
/// Returns Err(Error::BadRequest) in release if condition is false.
#[macro_export]
macro_rules! validate {
    ($cond:expr, $msg:expr) => {
        if !$cond {
            debug_assert!($cond, "validation failed: {}", $msg);
            return Err($crate::error::Error::BadRequest($msg.to_string()));
        }
    };
}
