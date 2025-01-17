pub struct Config {
    allow: Vec<String>,
    deny: Vec<String>,
}

// TODO 关于所有的 allow deny 规则的优先级该如何确定?
// allow 优先级应该要高于 deny 规则
// 全局级 allow 和 deny 规则
// server级 allow 和 deny 规则
// location 级 allow 和 deny 规则
