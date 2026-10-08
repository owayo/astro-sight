import Foundation

func displayName(_ info: [String: Any]) -> String {
  let name =
    info["displayName"] as? String
    ?? info["name"] as? String
    ?? "Example"
  return name
}

func afterChain(_ info: [String: Any]) -> String {
  return displayName(info)
}
