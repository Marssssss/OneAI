// OneAiErrorView → readable Chinese hint (port of Android friendlyError).

func friendlyError(_ e: OneAiErrorView) -> String {
    let raw: String
    switch e {
    case .Provider(let m), .Parser(let m), .Tool(let m), .Memory(let m),
         .Workflow(let m), .Agent(let m), .Skill(let m), .Scheduler(let m),
         .Persistence(let m), .Rag(let m), .Config(let m), .Serialization(let m),
         .Network(let m), .Timeout(let m), .Platform(let m), .Wasm(let m),
         .Other(let m):
        raw = m
    }
    switch e {
    case .Provider:      return "模型服务报错(检查 api key / model / 网络): \(raw)"
    case .Network:       return "网络不通(检查代理 / baseUrl): \(raw)"
    case .Timeout:       return "请求超时,可点重试"
    case .Config:        return "配置错误: \(raw)"
    case .Agent:         return "Agent 执行出错: \(raw)"
    case .Persistence:   return "持久化出错: \(raw)"
    case .Tool:          return "工具执行出错: \(raw)"
    default:             return raw
    }
}

func friendlyError(_ e: Error) -> String {
    if let v = e as? OneAiErrorView { return friendlyError(v) }
    return e.localizedDescription
}
