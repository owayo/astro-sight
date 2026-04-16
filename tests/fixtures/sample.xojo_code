Class Greeter
Inherits Object
  Const DefaultName As String = "world"

  Property Counter As Integer

  Sub Constructor()
    Me.Counter = 0
  End Sub

  Function Greet(name As String) As String
    Me.Counter = Me.Counter + 1
    Return "Hello, " + name
  End Function

  Sub PrintGreeting(name As String)
    Dim message As String = Me.Greet(name)
    Print(message)
  End Sub
End Class

Module Helpers
  Function MakeMessage(name As String) As String
    Return "Hi, " + name
  End Function
End Module
