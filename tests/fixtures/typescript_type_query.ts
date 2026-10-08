async function load(importOriginal: <T>() => Promise<T>) {
  const actual = await importOriginal<typeof import("node:fs")>()
  return actual
}

export const handlers = {
  read() { return load(importOriginal) },
  close() { return "closed" },
}
