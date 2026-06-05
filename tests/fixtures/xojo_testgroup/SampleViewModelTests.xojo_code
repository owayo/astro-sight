#tag Class
Protected Class SampleViewModelTests
Inherits TestGroup
	#tag Method, Flags = &h0
		Sub enableControlTest()
		  Assert.AreEqual(1, 1, "ok")
		End Sub
	#tag EndMethod
	#tag Method, Flags = &h0
		Sub validateValueTest()
		  Assert.IsTrue(True, "valid")
		End Sub
	#tag EndMethod
	#tag Method, Flags = &h0
		Sub Setup()
		  mModel = New Object
		End Sub
	#tag EndMethod
	#tag Method, Flags = &h0
		Sub orphanHelper()
		  // XojoUnit のテストではない通常メソッド。誰からも参照されなければ
		  // dead に残るべき (修正が「一律除外」でないことの退行検出用)。
		End Sub
	#tag EndMethod
#tag EndClass
