import Foundation

class TaskManager {
  private var tasks: [String] = []

  func addTask(_ task: String) {
    tasks.append(task)
  }

  func removeTask(at index: Int) -> String? {
    guard index >= 0 && index < tasks.count else {
      return nil
    }
    return tasks.remove(at: index)
  }

  func listTasks() -> [String] {
    return tasks
  }

  var taskCount: Int {
    return tasks.count
  }
}
