//! 內建工具(design §7、D-044):都沒有副作用,不需要沙箱。

mod calculator;
mod clock;

pub use calculator::{Calculator, CalculatorArgs};
pub use clock::{CurrentTime, CurrentTimeArgs};
