#tag Class
Protected Class DesktopTestController
Inherits TestController
	#tag Event
		Sub InitializeTestGroups()
		  // XojoUnit が #tag Event で呼ぶ entrypoint。
		  // テストクラスをここで New 登録する (各クラスは New で参照される)。
		  Var group As TestGroup
		  group = New SampleViewModelTests(Self, "SampleViewModelTests")
		  group = New DerivedViewModelTests(Self, "DerivedViewModelTests")
		End Sub
	#tag EndEvent
#tag EndClass
