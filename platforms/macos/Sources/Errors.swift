// OneAiErrorView → readable Chinese hint (port of Android friendlyError).

import Foundation

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
    case .Provider:      return String(format: NSLocalizedString("模型服务报错(检查 api key / model / 网络): %@", comment: ""), raw)
    case .Network:       return String(format: NSLocalizedString("网络不通(检查代理 / baseUrl): %@", comment: ""), raw)
    case .Timeout:       return NSLocalizedString("请求超时,可点重试", comment: "")
    case .Config:        return String(format: NSLocalizedString("配置错误: %@", comment: ""), raw)
    case .Agent:         return String(format: NSLocalizedString("Agent 执行出错: %@", comment: ""), raw)
    case .Persistence:   return String(format: NSLocalizedString("持久化出错: %@", comment: ""), raw)
    case .Tool:          return String(format: NSLocalizedString("工具执行出错: %@", comment: ""), raw)
    default:             return raw
    }
}

func friendlyError(_ e: Error) -> String {
    if let v = e as? OneAiErrorView { return friendlyError(v) }
    return e.localizedDescription
}
