struct Loader {
  func names() async -> [String] {
    if let group = try? await loadGroup() {
      let options = group.map(\.description)
      if !options.isEmpty { return options }
    }
    return await fallback()
  }

  func loadGroup() async throws -> [Int]? { nil }
  func fallback() async -> [String] { [] }
}
