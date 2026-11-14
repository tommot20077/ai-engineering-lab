//! 內建工具(design §7、D-044):都沒有副作用,不需要沙箱。

mod clock;

pub use clock::{CurrentTime, CurrentTimeArgs};
