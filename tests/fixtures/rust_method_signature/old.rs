use std::borrow::Cow;

struct State {
    blanked: Vec<usize>,
}

impl State {
    fn apply<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.blanked.is_empty() {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(text.to_owned())
        }
    }
}
