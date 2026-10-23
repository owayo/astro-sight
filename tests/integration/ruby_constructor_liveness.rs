use super::support::TestRepo;

const ISSUE_SOURCE: &str = "module Demo\n  class Builder\n    def initialize(input)\n      @input = input\n    end\n    def output\n      @input\n    end\n    def unused\n      nil\n    end\n  end\n  def self.build(input)\n    Builder.new(input).output\n  end\nend\nputs Demo.build('ok')\n";

fn contains_name(value: &serde_json::Value, key: &str, name: &str) -> bool {
    value[key]
        .as_array()
        .is_some_and(|symbols| symbols.iter().any(|symbol| symbol["name"] == name))
}

#[test]
fn ruby_constructor_liveness_is_shared_by_dead_code_and_review_without_changing_refs() {
    let repo = TestRepo::new();
    repo.write("example.rb", ISSUE_SOURCE);
    repo.init_git();
    repo.commit_all("initial");
    let before = repo.run_json("refs", &["--name", "initialize"]);
    assert_eq!(before["refs"].as_array().unwrap().len(), 1, "{before}");
    assert_eq!(before["refs"][0]["kind"], "def");

    let dead = repo.run_json("dead-code", &[]);
    assert!(
        !contains_name(&dead, "dead_symbols", "Builder.initialize"),
        "{dead}"
    );
    assert!(
        !contains_name(&dead, "test_only_symbols", "Builder.initialize"),
        "{dead}"
    );
    assert!(
        contains_name(&dead, "dead_symbols", "Builder.unused"),
        "{dead}"
    );

    repo.write(
        "example.rb",
        ISSUE_SOURCE.replace("@input = input", "@input = input.to_s"),
    );
    let review = repo.run_json("review", &["--git", "--dead-scope", "all"]);
    assert!(
        !contains_name(&review, "dead_symbols", "Builder.initialize"),
        "{review}"
    );
    assert!(
        contains_name(&review, "dead_symbols", "Builder.unused"),
        "{review}"
    );
    let after = repo.run_json("refs", &["--name", "initialize"]);
    assert_eq!(before["refs"], after["refs"]);
}

#[test]
fn ruby_constructor_liveness_counts_tests_and_generated_callers_outside_the_candidate_glob() {
    let repo = TestRepo::new();
    repo.write("builder.rb", "class Builder\n  def initialize; end\nend\n");
    repo.create_dir_all("tests");
    repo.write("tests/build.rb", "Builder.new\n");
    let dead = repo.run_json("dead-code", &["--glob", "builder.rb"]);
    assert!(
        !contains_name(&dead, "dead_symbols", "Builder.initialize"),
        "{dead}"
    );
    assert!(
        contains_name(&dead, "test_only_symbols", "Builder.initialize"),
        "{dead}"
    );

    repo.write("generated.rb", "# @generated\nBuilder.new\n");
    let live = repo.run_json("dead-code", &["--glob", "builder.rb"]);
    assert!(
        !contains_name(&live, "dead_symbols", "Builder.initialize"),
        "{live}"
    );
    assert!(
        !contains_name(&live, "test_only_symbols", "Builder.initialize"),
        "{live}"
    );
}

#[test]
fn ruby_constructor_liveness_does_not_blanket_exclude_initializers() {
    let repo = TestRepo::new();
    repo.write(
        "builder.rb",
        "class Builder\n  def initialize; end\nend\nputs Builder\n",
    );
    let dead = repo.run_json("dead-code", &[]);
    assert!(
        contains_name(&dead, "dead_symbols", "Builder.initialize"),
        "{dead}"
    );
    repo.write(
        "builder.rb",
        "class Builder\n  Builder.new\n  def initialize; end\nend\n",
    );
    let early = repo.run_json("dead-code", &[]);
    assert!(
        contains_name(&early, "dead_symbols", "Builder.initialize"),
        "{early}"
    );
}

#[test]
fn ruby_constructor_liveness_does_not_hide_overwritten_or_unevaluated_initializers() {
    for (source, name) in [
        (
            "class Builder; def initialize; raise 'unused'; end; end; defined?(Builder.new)",
            "Builder.initialize",
        ),
        (
            "class Builder; def (Builder.new).marker; end; def initialize; raise 'unused'; end; end",
            "Builder.initialize",
        ),
        (
            "class Builder; def initialize; raise 'unused'; end; attr_reader('initial' + 'ize'); end; Builder.new",
            "Builder.initialize",
        ),
        (
            "class Struct; def initialize; raise 'unused'; end; end; Struct.new(:value)",
            "Struct.initialize",
        ),
    ] {
        let repo = TestRepo::new();
        repo.write("example.rb", source);
        let dead = repo.run_json("dead-code", &[]);
        assert!(
            contains_name(&dead, "dead_symbols", name),
            "{source}: {dead}"
        );
    }
}
