func consume(_ value: Void) {}
func consumeInt(_ value: Int) {}

let globalUnit: Void = ()

func produce() -> Result<Void, Error> {
  let unit: Void = ()
  consume(unit)
  consume(())
  return .success(())
}

func variants() {
  var assigned: Void = ()
  assigned = ()
  let wrapped: Void = (())
  let pair: (Void, Int) = ((), 1)
  let values: [Void] = [(), ()]
  let closure: () -> Void = { () }
  consume(wrapped)
  consume(pair.0)
  consume(values[0])
  consume(closure())
  consume(assigned)
}

func inspect(_ value: Result<Void, Error>) {
  switch value {
  case .success(()): break
  case .failure: break
  }
}

func controls() {
  consume(Void())
  consumeInt((1))
  let closure: () -> Void = { () in }
  consume(closure())
}

func consumeLabeled(value: Void) {}

func directReturn() {
  return ()
}

func asyncConsume(_ value: Result<Void, Error>) async {}

func asyncVariants() async {
  consumeLabeled(value: ())
  await asyncConsume(.success(()))
}
