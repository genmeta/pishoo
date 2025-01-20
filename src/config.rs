// TODO 只区分 全局 allow 和 server allow
#[allow(dead_code)]
pub struct Config {
    allow: Vec<String>,
    deny: Vec<String>,
}
