#tag Window
Begin Window MainWindow
End
#tag EndWindow
#tag WindowCode
  Sub Open()
    Dim c As Caller = New Caller
    c.Run()
  End Sub
  Sub Close()
  End Sub
#tag EndWindowCode
#tag Events Listbox1
  #tag Event
    Sub CellAction(row As Integer, column As Integer)
      Print("cell clicked")
    End Sub
  #tag EndEvent
#tag EndEvents
