use std::borrow::Cow;

struct State {
    blanked: Vec<usize>,
}

impl State {
    fn apply<'a>(&self, text: &'a str) -> Readable<'a> {
        let analysis = if self.blanked.is_empty() {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(text.to_owned())
        };
        Readable { analysis }
    }
}

struct Readable<'a> {
    analysis: Cow<'a, str>,
}

impl Readable<'_> {
    fn text(&self) -> &str {
        &self.analysis
    }
}
