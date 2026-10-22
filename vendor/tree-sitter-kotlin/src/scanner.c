#include <string.h>
#include <tree_sitter/parser.h>
#include <wctype.h>

// Mostly a copy paste of tree-sitter-javascript/src/scanner.c

enum TokenType {
  AUTOMATIC_SEMICOLON,
  IMPORT_LIST_DELIMITER,
  SAFE_NAV,
  MULTILINE_COMMENT,
  STRING_START,
  STRING_END,
  STRING_CONTENT,
  WHEN_ENTRY_SEPARATOR,
};

/* Pretty much all of this code is taken from the Julia tree-sitter
   parser.

   Julia has similar problems with multiline comments that can be nested,
   line comments, as well as line and multiline strings.

   The most heavily edited section is `scan_string_content`,
   particularly with respect to interpolation.
 */

typedef char Delimiter;

// Block comments are easy to parse, but strings require extra-attention.

// The main problems that arise when parsing strings are:
// 1. Triple quoted strings allow single quotes inside. e.g. """ "foo" """.
// 2. Non-standard string literals don't allow interpolations or escape
//    sequences, but you can always write \" and \`.

// To efficiently store a delimiter, we take advantage of the fact that:
// (int)'"' == 34 && 34 % 2 == 0
// i.e. " has an even numeric representation, so we can store a triple
// quoted delimiter as (delimiter + 1).

// We use a stack to keep track of the string delimiters.
typedef struct {
  Delimiter *arr;
  unsigned len;
} Stack;

static Stack *new_stack() {
  Delimiter *arr = malloc(TREE_SITTER_SERIALIZATION_BUFFER_SIZE);
  if (arr == NULL) abort();
  Stack *stack = malloc(sizeof(Stack));
  if (stack == NULL) abort();
  stack->arr = arr;
  stack->len = 0;
  return stack;
}

static void free_stack(Stack *stack) {
  free(stack->arr);
  free(stack);
}

static void push(Stack *stack, char chr, bool triple) {
  if (stack->len >= TREE_SITTER_SERIALIZATION_BUFFER_SIZE) abort();
  stack->arr[stack->len++] = (Delimiter)(triple ? (chr + 1) : chr);
}

static Delimiter pop(Stack *stack) {
  if (stack->len == 0) abort();
  return stack->arr[stack->len--];
}

static unsigned serialize_stack(Stack *stack, char *buffer) {
  unsigned len = stack->len;
  memcpy(buffer, stack->arr, len);
  return len;
}

static void deserialize_stack(Stack *stack, const char *buffer, unsigned len) {
  if (len > 0) {
    memcpy(stack->arr, buffer, len);
    stack->len = len;
  } else {
    stack->len = 0;
  }
}

static void skip(TSLexer *lexer) { lexer->advance(lexer, true); }
static void advance(TSLexer *lexer) { lexer->advance(lexer, false); }
static void mark_end(TSLexer *lexer) { lexer->mark_end(lexer); }

// Scanner functions

static bool scan_string_start(TSLexer *lexer, Stack *stack) {
  if (lexer->lookahead != '"') return false;
  advance(lexer);
  mark_end(lexer);
  for (unsigned count = 1; count < 3; count++) {
    if (lexer->lookahead != '"') {
      // It's not a triple quoted delimiter.
      push(stack, '"', false);
      return true;
    }
    advance(lexer);
  }
  mark_end(lexer);
  push(stack, '"', true);
  return true;
}

static bool scan_string_content(TSLexer *lexer, Stack *stack) {
  if (stack->len == 0) return false;  // Stack is empty. We're not in a string.
  Delimiter end_char = stack->arr[stack->len - 1];  // peek
  bool is_triple = false;
  bool has_content = false;
  if (end_char % 2 != 0) {
    is_triple = true;
    end_char--;
  }
  while (lexer->lookahead) {
    if (lexer->lookahead == '$') {
      // if we did not just start reading stuff, then we should stop
      // lexing right here, so we can offer the opportunity to lex a
      // interpolated identifier
      if (has_content) {
        lexer->result_symbol = STRING_CONTENT;
        return has_content;
      }
      // otherwise, if this is the start, determine if it is an 
      // interpolated identifier.
      // otherwise, it's just string content, so continue 
      advance(lexer);
      if (iswalpha(lexer->lookahead) || lexer->lookahead == '{') {
        // this must be a string interpolation, let's
        // fail so we parse it as such
        return false;
      }
      lexer->result_symbol = STRING_CONTENT;
      mark_end(lexer);
      return true;
    } 
    if (lexer->lookahead == '\\') {
      // if we see a \, then this might possibly escape a dollar sign
      // in which case, we need to not defer to the interpolation 
      has_content = true;
      advance(lexer);
      // this dollar sign is escaped, so it must be content.
      // we consume it here so we don't enter the dollar sign case above,
      // which leaves the possibility that it is an interpolation 
      if (lexer->lookahead == '$') {
        advance(lexer);
      }
    } else if (lexer->lookahead == end_char) {
      if (is_triple) {
        mark_end(lexer);
        for (unsigned count = 1; count < 3; count++) {
          advance(lexer);
          if (lexer->lookahead != end_char) {
            mark_end(lexer);
            lexer->result_symbol = STRING_CONTENT;
            return true;
          }
        }

        /* This is so if we lex something like 
           """foo"""
              ^ 
           where we are at the `f`, we should quit after
           reading `foo`, and ascribe it to STRING_CONTENT.

           Then, we restart and try to read the end.
           This is to prevent `foo` from being absorbed into
           the STRING_END token.
         */
        if (has_content && lexer->lookahead == end_char) {
          lexer->result_symbol = STRING_CONTENT;
          return true;
        }

        /* Since the string internals are all hidden in the syntax
           tree anyways, there's no point in going to the effort of 
           specifically separating the string end from string contents.
           If we see a bunch of quotes in a row, then we just go until
           they stop appearing, then stop lexing and call it the
           string's end.
         */
        lexer->result_symbol = STRING_END;
        mark_end(lexer);
        while (lexer->lookahead == end_char) {
          advance(lexer);
          mark_end(lexer);
        }
        pop(stack);
        return true;
      }
      if (has_content) {
        mark_end(lexer);
        lexer->result_symbol = STRING_CONTENT;
        return true;
      }
      pop(stack);
      advance(lexer);
      mark_end(lexer);
      lexer->result_symbol = STRING_END;
      return true;
    }
    advance(lexer);
    has_content = true;
  }
  return false;
}

static bool scan_multiline_comment(TSLexer *lexer) {
  if (lexer->lookahead != '/') return false;
  advance(lexer);
  if (lexer->lookahead != '*') return false;
  advance(lexer);

  bool after_star = false;
  unsigned nesting_depth = 1;
  for (;;) {
    switch (lexer->lookahead) {
      case '*':
        advance(lexer);
        after_star = true;
        break;
      case '/':
        advance(lexer);
        if (after_star) {
          after_star = false;
          nesting_depth--;
          if (nesting_depth == 0) {
            lexer->result_symbol = MULTILINE_COMMENT;
            mark_end(lexer);
            return true;
          }
        } else {
          after_star = false;
          if (lexer->lookahead == '*') {
            nesting_depth++;
            advance(lexer);
          }
        }
        break;
      case '\0':
        return false;
      default:
        advance(lexer);
        after_star = false;
        break;
    }
  }
}

static bool scan_whitespace_and_comments(TSLexer *lexer) {
  for (;;) {
    while (iswspace(lexer->lookahead)) {
      skip(lexer);
    }

    if (lexer->lookahead == '/') {
      return false;
    }
    return true;
  }
}

static bool scan_for_word(TSLexer *lexer, const char* word, unsigned len) {
    skip(lexer);
    for (unsigned i = 0; i < len; i++) {
      if (lexer->lookahead != word[i]) return false;
      skip(lexer);
    }
    return true;
}

// Look ahead without changing mark_end: the separator is zero width and the
// ordinary parser must still consume the complete following condition.
static bool scan_when_layout(TSLexer *lexer, bool *newline) {
  for (;;) {
    while (iswspace(lexer->lookahead)) {
      *newline |= lexer->lookahead == '\n' || lexer->lookahead == '\r';
      skip(lexer);
    }
    if (lexer->lookahead != '/') return true;
    advance(lexer);
    if (lexer->lookahead == '/') {
      while (!lexer->eof(lexer) && lexer->lookahead != '\n' && lexer->lookahead != '\r') advance(lexer);
    } else if (lexer->lookahead == '*') {
      advance(lexer);
      unsigned depth = 1;
      while (depth && !lexer->eof(lexer)) {
        int32_t c = lexer->lookahead;
        *newline |= c == '\n' || c == '\r';
        advance(lexer);
        if (c == '/' && lexer->lookahead == '*') { advance(lexer); depth++; }
        else if (c == '*' && lexer->lookahead == '/') { advance(lexer); depth--; }
      }
      if (depth) return false;
    } else {
      return false;
    }
  }
}

static bool scan_when_quoted(TSLexer *lexer) {
  int32_t quote = lexer->lookahead;
  advance(lexer);
  bool triple = false;
  if (quote == '"' && lexer->lookahead == '"') {
    advance(lexer);
    if (lexer->lookahead != '"') return true;
    advance(lexer);
    triple = true;
  }
  unsigned quotes = 0;
  while (!lexer->eof(lexer)) {
    int32_t c = lexer->lookahead;
    // Interpolation can contain nested Kotlin strings; leave such conditions
    // to the original grammar rather than infer a boundary inside them.
    if (quote == '"' && c == '$') return false;
    if (!triple && (c == '\n' || c == '\r')) return false;
    advance(lexer);
    if (!triple && c == '\\' && quote != '`') {
      if (lexer->eof(lexer)) return false;
      advance(lexer);
      continue;
    }
    if (c == quote) {
      if (!triple) return true;
      if (++quotes == 3) {
        // Match the existing string scanner: a raw string's closing run
        // includes all consecutive quotes, including runs of four or five.
        while (lexer->lookahead == quote) advance(lexer);
        return true;
      }
    } else {
      quotes = 0;
    }
  }
  return false;
}

static bool scan_when_condition_arrow(TSLexer *lexer) {
  char closers[128];
  unsigned depth = 0;
  bool has_token = false;
  for (unsigned count = 0; count < 65536 && !lexer->eof(lexer); count++) {
    bool newline = false;
    bool at_next_line = false;
    if (iswspace(lexer->lookahead) || lexer->lookahead == '/') {
      if (!scan_when_layout(lexer, &newline)) return false;
      at_next_line = newline && depth == 0 && has_token;
      // A later branch must not turn an ordinary multiline call into an
      // entry. Only the arrow or a Kotlin line-leading continuation may follow.
      if (newline && depth == 0 && has_token && lexer->lookahead != '-'
          && lexer->lookahead != '&' && lexer->lookahead != '|'
          && lexer->lookahead != '?') return false;
    }
    int32_t c = lexer->lookahead;
    if (c == '-') {
      advance(lexer);
      if (lexer->lookahead == '>') {
        if (depth == 0) return has_token;
        advance(lexer);
        has_token = true;
        continue;
      }
      // A minus in this condition is ordinary syntax. Across a newline it
      // may instead start the next branch; do not borrow that branch's arrow.
      if (depth == 0 && at_next_line) return false;
      // Kotlin lexes '-->' as postfix '--' followed by comparison '>', not
      // a minus followed by a branch arrow. Respect that maximal munch.
      if (lexer->lookahead == '-') advance(lexer);
      has_token = true;
      continue;
    }
    if (c == ';' || (c == '}' && depth == 0)) return false;
    if (depth == 0 && (iswalpha(c) || c == '_')) {
      char word[7] = {0};
      unsigned length = 0;
      do {
        if (length < 6) word[length] = lexer->lookahead;
        length++;
        advance(lexer);
      } while (iswalnum(lexer->lookahead) || lexer->lookahead == '_');
      // Casts, type checks and anonymous functions/objects can introduce a
      // function-type arrow at this depth.
      // Resolving that type grammar is outside this boundary proof.
      if ((length == 2 && (!strcmp(word, "as") || !strcmp(word, "is")))
          || (length == 3 && !strcmp(word, "fun"))
          || (length == 6 && !strcmp(word, "object"))) return false;
      has_token = true;
      continue;
    }
    if (c == '"' || c == '\'' || c == '`') {
      if (!scan_when_quoted(lexer)) return false;
      has_token = true;
      continue;
    }
    if (c == '<' && (depth == 0 || closers[0] == '>')) {
      // Keep arrows in explicit type arguments inside their angle brackets.
      // An unmatched comparison '<' conservatively leaves the old behavior.
      if (depth == sizeof(closers)) return false;
      closers[depth++] = '>';
    } else if (c == '>' && depth && closers[depth - 1] == '>') {
      depth--;
    } else if (c == '(' || c == '[' || c == '{') {
      if (depth == sizeof(closers)) return false;
      closers[depth++] = c == '(' ? ')' : c == '[' ? ']' : '}';
    } else if (c == ')' || c == ']' || c == '}') {
      if (!depth || closers[--depth] != c) return false;
    }
    has_token = true;
    advance(lexer);
  }
  return false;
}

static bool scan_when_entry_start(TSLexer *lexer) {
  if (lexer->lookahead == '(') return scan_when_condition_arrow(lexer);
  if (lexer->lookahead != 'e' || !scan_for_word(lexer, "lse", 3)) return false;
  bool newline = false;
  if (!scan_when_layout(lexer, &newline) || lexer->lookahead != '-') return false;
  advance(lexer);
  return lexer->lookahead == '>';
}

static bool scan_automatic_semicolon(TSLexer *lexer, bool when_separator, bool *probed_when) {
  lexer->result_symbol = AUTOMATIC_SEMICOLON;
  lexer->mark_end(lexer);

  bool sameline = true;
  for (;;) {
    if (lexer->eof(lexer))
      return true;

    if (lexer->lookahead == ';') {
      advance(lexer);
      lexer->mark_end(lexer);
      return true;
    }

    if (!iswspace(lexer->lookahead)) {
      break;
    }

    if (lexer->lookahead == '\n') {
      skip(lexer);

      sameline = false;
      break;
    }

    if (lexer->lookahead == '\r') {
      skip(lexer);

      if (lexer->lookahead == '\n') {
        skip(lexer);
      }

      sameline = false;
      break;
    }

    skip(lexer);
  }

  // Comments are extras. Probe across them only for the new token; if the
  // probe fails, let the parser consume the extras from the original position.
  // Never offer another external token from this speculative future position.
  // Skip whitespace and comments
  if (!scan_whitespace_and_comments(lexer)) {
    if (when_separator) {
      bool newline = false;
      *probed_when = true;
      if (scan_when_layout(lexer, &newline) && (!sameline || newline)
          && scan_when_entry_start(lexer)) {
        lexer->result_symbol = WHEN_ENTRY_SEPARATOR;
        return true;
      }
    }
    return false;
  }

  if (sameline) {
    switch (lexer->lookahead) {
      // Don't insert a semicolon before an else
      case 'e':
        return !scan_for_word(lexer, "lse", 3);

      case 'i':
        return scan_for_word(lexer, "mport", 5);

      case ';':
        advance(lexer);
        lexer->mark_end(lexer);
        return true;

      default:
        return false;
    }
  }

  switch (lexer->lookahead) {
    case ',':
    case '.':
    case ':':
    case '*':
    case '%':
    case '>':
    case '<':
    case '=':
    case '{':
    case '[':
    case '?':
    case '|':
    case '&':
    case '/':
      return false;

    case '(':
      if (when_separator) {
        *probed_when = true;
        if (scan_when_condition_arrow(lexer)) {
          lexer->result_symbol = WHEN_ENTRY_SEPARATOR;
          return true;
        }
      }
      return false;
    
    // Insert a semicolon before `--` and `++`, but not before binary `+` or `-`.
    // Insert before +/-Float
    case '+':
      skip(lexer);
      if (lexer->lookahead == '+')
        return true;
      return iswdigit(lexer->lookahead);
    case '-':
      skip(lexer);
      if (lexer->lookahead == '-')
        return true;
      return iswdigit(lexer->lookahead);

    // Don't insert a semicolon before `!=`, but do insert one before a unary `!`.
    case '!':
      skip(lexer);
      return lexer->lookahead != '=';

    // Don't insert a semicolon before an else
    case 'e':
      if (!scan_for_word(lexer, "lse", 3)) return true;
      if (when_separator) {
        *probed_when = true;
        bool newline = false;
        if (scan_when_layout(lexer, &newline) && lexer->lookahead == '-') {
          advance(lexer);
          if (lexer->lookahead == '>') {
            lexer->result_symbol = WHEN_ENTRY_SEPARATOR;
            return true;
          }
        }
      }
      return false;

    // Don't insert a semicolon before `in` or `instanceof`, but do insert one
    // before an identifier or an import.
    case 'i':
      skip(lexer);
      if (lexer->lookahead != 'n')
        return true;

      skip(lexer);
      if (!iswalpha(lexer->lookahead))
        return false;

      return !scan_for_word(lexer, "stanceof", 8);

      case ';':
        advance(lexer);
        lexer->mark_end(lexer);
        return true;

    default:
      return true;
  }
}

static bool scan_safe_nav(TSLexer *lexer) {
  lexer->result_symbol = SAFE_NAV;
  lexer->mark_end(lexer);

  // skip white space
  if (!scan_whitespace_and_comments(lexer))
    return false;

  if (lexer->lookahead != '?')
    return false;

  advance(lexer);

  if (!scan_whitespace_and_comments(lexer))
    return false;

  if (lexer->lookahead != '.')
    return false;

  advance(lexer);
  lexer->mark_end(lexer);
  return true;
}

static bool scan_line_sep(TSLexer *lexer) {
  // Line Seps: [ CR, LF, CRLF ]
  int state = 0;
  while (true) {
    switch(lexer->lookahead) {
      case  ' ':
      case '\t':
      case '\v':
        // Skip whitespace
        advance(lexer);
        break;

      case '\n':
        advance(lexer);
        return true;

      case '\r':
        if (state == 1)
          return true;

        state = 1;
        advance(lexer);
        break;

      default:
        // We read a CR
        if (state == 1)
          return true;

        return false;
    }
  }
}

static bool scan_import_list_delimiter(TSLexer *lexer) {
  // Import lists are terminated either by an empty line or a non import statement
  lexer->result_symbol = IMPORT_LIST_DELIMITER;
  lexer->mark_end(lexer);

  // if eof; return true
  if (lexer->eof(lexer))
    return true;

  // Scan for the first line seperator
  if (!scan_line_sep(lexer))
    return false;

  // if line.sep line.sep; return true
  if (scan_line_sep(lexer)) {
    lexer->mark_end(lexer);
    return true;
  }

  // if line.sep [^import]; return true
  while (true) {
    switch (lexer->lookahead) {
      case  ' ':
      case '\t':
      case '\v':
        // Skip whitespace
        advance(lexer);
        break;

      case 'i':
        return !scan_for_word(lexer, "mport", 5);

      default:
        return true;
    }

    return false;
  }
}

bool tree_sitter_kotlin_external_scanner_scan(void *payload, TSLexer *lexer,
                                                  const bool *valid_symbols) {

  bool *in_string = payload;

  if (valid_symbols[AUTOMATIC_SEMICOLON] || valid_symbols[WHEN_ENTRY_SEPARATOR]) {
    bool probed_when = false;
    bool ret = scan_automatic_semicolon(lexer,
        valid_symbols[WHEN_ENTRY_SEPARATOR] && !valid_symbols[STRING_CONTENT], &probed_when);
    if (ret) return valid_symbols[lexer->result_symbol];
    if (probed_when) return false;
    if (!ret && valid_symbols[SAFE_NAV] && lexer->lookahead == '?')
      return scan_safe_nav(lexer);

    // if we fail to find an automatic semicolon, it's still possible that we may 
    // want to lex a string or comment later
    if (ret)
      return ret;
  }

  if (valid_symbols[IMPORT_LIST_DELIMITER])
    return scan_import_list_delimiter(lexer);

  // content or end
  if (valid_symbols[STRING_CONTENT] &&
      scan_string_content(lexer, payload)) {
    return true;
  }

  // a string might follow after some whitespace, so we can't lookahead 
  // until we get rid of it
  while(iswspace(lexer->lookahead)) {
    skip(lexer);
  }

  if (valid_symbols[STRING_START] && scan_string_start(lexer, payload)) {
    lexer->result_symbol = STRING_START;
    return true;
  }

  if (valid_symbols[MULTILINE_COMMENT] && scan_multiline_comment(lexer)) {
    return true;
  }

  if (valid_symbols[SAFE_NAV]) {
    return scan_safe_nav(lexer);
  }

  return false;
}

void *tree_sitter_kotlin_external_scanner_create() { return new_stack(); }

void tree_sitter_kotlin_external_scanner_destroy(void *payload) {
  free_stack(payload);
}

unsigned tree_sitter_kotlin_external_scanner_serialize(
    void *payload,
    char *buffer
) {
  return serialize_stack(payload, buffer);
}

void tree_sitter_kotlin_external_scanner_deserialize(
    void *payload,
    const char *buffer,
    unsigned length
) {
  deserialize_stack(payload, buffer, length);
}
