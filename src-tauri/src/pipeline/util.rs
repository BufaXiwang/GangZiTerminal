//! 通用小工具——id / 时间戳。chat / news 用例里反复要这两个。

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
