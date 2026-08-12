values = {}
values [:key] = 1

matcher = -> item=item_for() { item }
empty_patterns = //..//

assert_pattern { node.at("h1") => { content: "x" } }

document = <<""
  body line

puts document
