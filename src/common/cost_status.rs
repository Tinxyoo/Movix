use std::sync::Mutex;
use std::sync::OnceLock;

/// 后台任务累加的待合并人民币费用(¥)。
static PENDING: OnceLock<Mutex<f64>> = OnceLock::new();

fn cell() -> &'static Mutex<f64> {
    PENDING.get_or_init(|| Mutex::new(0.0))
}

/// 将后台任务的人民币费用(¥)累加到待合并池。
/// 入参为 0 或负数时直接忽略。
pub fn report(cny: f64) {
    if cny <= 0.0 {
        return;
    }
    if let Ok(mut pending) = cell().lock() {
        *pending += cny;
    }
}

/// 取出并清空当前待合并的费用(¥)。
pub fn drain() -> f64 {
    let Ok(mut pending) = cell().lock() else {
        return 0.0;
    };
    std::mem::take(&mut *pending)
}

#[cfg(test)]
pub fn reset_for_tests() {
    if let Ok(mut pending) = cell().lock() {
        *pending = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn test_report_and_drain() {
        let _g = serial_lock();
        reset_for_tests();
        report(0.07);
        report(0.14);
        let cost = drain();
        assert!((cost - 0.21).abs() < 0.001, "drain cny: {}", cost);

        let after = drain();
        assert_eq!(after, 0.0);
    }

    #[test]
    fn test_report_ignores_zero() {
        let _g = serial_lock();
        reset_for_tests();
        report(0.0);
        report(-0.5);
        assert_eq!(drain(), 0.0);
    }
}
