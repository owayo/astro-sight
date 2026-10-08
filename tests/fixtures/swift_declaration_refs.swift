protocol Renderer {
  var required: Int { get }
  func draw() -> Int
}

typealias Pixels = Int

enum Quality {
  case draft, final
}

class Canvas: Renderer {
  static let maxRetries = 3
  static var scale = 2
  static var computedScale: Int { scale }
  class var sharedScale: Int { scale }
  var total = 0
  let label: String = "idle"
  lazy var cached = 4
  var required: Int { total }
  func draw() -> Int { required }
}

let globalScale = 2
let first = 1, second = 2
let (left, right) = (3, 4)
let (x:labeledLeft, y:labeledRight) = (x: 5, y: 6)

func paint(_ q: Quality) -> Pixels {
  let local = first
  var mutable = local
  mutable += second
  switch q {
  case .draft: return Canvas.maxRetries * globalScale + mutable + Canvas().draw()
  case .final: return left + right + labeledLeft + labeledRight
  }
}
