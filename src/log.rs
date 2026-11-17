//! Timestamped log lines on stderr (UTC). Keys and signed bytes are never logged.

pub fn emit(level: &str, args: std::fmt::Arguments<'_>) {
    eprintln!("{} {level:<5} {args}", crate::clock::fmt_utc(crate::clock::now_unix()));
}

#[macro_export]
macro_rules! info {
    ($($t:tt)*) => { $crate::log::emit("INFO", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! warn {
    ($($t:tt)*) => { $crate::log::emit("WARN", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! error {
    ($($t:tt)*) => { $crate::log::emit("ERROR", format_args!($($t)*)) };
}

#[macro_export]
macro_rules! trade {
    ($($t:tt)*) => { $crate::log::emit("TRADE", format_args!($($t)*)) };
}
