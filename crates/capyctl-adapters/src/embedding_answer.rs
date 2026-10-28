//! ADR 0036 §4: the incremental parser an embeddings answer passes through on
//! its way to the client. It knows one fixed JSON shape and keeps only its own
//! state, never the answer: each byte is checked as it arrives and handed back
//! to be relayed, with the engine's `model` replaced by the public name. The
//! top-level object's closing brace, and anything after it, is never handed
//! back: the caller writes `capyctl.metrics` and the brace once every check
//! has passed (§4, §9).
//!
//! The shape: a top-level object with `object` (`"list"`), `data`, `model` and
//! `usage`, in any order; `data` an array of entry objects, each with
//! `object`, `index` and `embedding`. Any other member, at the top level or in
//! an entry, is relayed unchanged once it parses as a JSON value.

/// ADR 0036 §4: the longest number the parser reads, in bytes.
pub(crate) const NUMBER_LIMIT: usize = 64;
/// ADR 0036 §4: the longest member name the parser reads, in bytes as sent.
pub(crate) const NAME_LIMIT: usize = 256;
/// ADR 0036 §4: the deepest nesting the parser reads, in levels.
pub(crate) const DEPTH_LIMIT: usize = 32;

/// The longest member name the parser compares: no name of the shape is
/// longer (`prompt_tokens`), so a longer one is an unknown member.
const KNOWN_NAME: usize = 16;

/// ADR 0036 §3: the vector encoding the request asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Float,
    Base64,
}

/// What the request makes the answer owe: one entry per input, each vector in
/// the asked encoding and, when `dimensions` was asked, of that length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Expected {
    pub(crate) inputs: usize,
    pub(crate) encoding: Encoding,
    pub(crate) dimensions: Option<usize>,
}

/// ADR 0036 §4: the answer broke its shape or a check. What was relayed
/// before stays relayed; nothing more is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Malformed(pub(crate) &'static str);

/// ADR 0036 §4: an answer that ended whole with every check passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Complete {
    /// `usage.prompt_tokens`, when the engine reported it.
    pub(crate) prompt_tokens: Option<u64>,
}

/// One open container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Frame {
    Top,
    Data,
    Entry,
    /// A `float` embedding.
    Vector,
    Usage,
    Object,
    Array,
}

impl Frame {
    fn is_object(self) -> bool {
        matches!(self, Self::Top | Self::Entry | Self::Usage | Self::Object)
    }

    /// What each item of this array is.
    fn item(self) -> Slot {
        match self {
            Self::Data => Slot::Entry,
            Self::Vector => Slot::Component,
            _ => Slot::Other,
        }
    }
}

/// What the next value is in the shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    TopObject,
    TopData,
    TopModel,
    TopUsage,
    Entry,
    EntryObject,
    EntryIndex,
    EntryEmbedding,
    PromptTokens,
    TotalTokens,
    Component,
    /// A member the shape does not name: any JSON value, relayed.
    Other,
}

/// What a string being read is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
    Name,
    Value(Slot),
}

/// Where a string is in an escape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Escape {
    None,
    Backslash,
    /// `\u` and the hexadecimal digits read so far; `high` is the leading
    /// surrogate this escape must complete.
    Hex {
        digits: u8,
        value: u32,
        high: Option<u32>,
    },
    /// A leading surrogate was read: `\` must follow.
    WantLow {
        high: u32,
    },
    /// A leading surrogate and `\` were read: `u` must follow.
    WantLowU {
        high: u32,
    },
}

/// Where a number is in JSON's grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Num {
    Minus,
    Zero,
    Int,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpDigits,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Start,
    /// A member name's opening quote, or `}` when `first`.
    Name {
        first: bool,
    },
    Colon,
    /// A value, or `]` when `first` (an array just opened).
    Value {
        slot: Slot,
        first: bool,
    },
    /// A comma or the current container's end.
    After,
    Text {
        text: Text,
        escape: Escape,
    },
    Number {
        slot: Slot,
        at: Num,
    },
    Literal {
        word: &'static [u8],
        at: usize,
    },
    /// The top-level object ended: whitespace only, never relayed.
    Done,
}

const TOP_OBJECT: u8 = 1;
const TOP_DATA: u8 = 2;
const TOP_MODEL: u8 = 4;
const TOP_USAGE: u8 = 8;
const ENTRY_OBJECT: u8 = 1;
const ENTRY_INDEX: u8 = 2;
const ENTRY_EMBEDDING: u8 = 4;
const USAGE_PROMPT: u8 = 1;
const USAGE_TOTAL: u8 = 2;

/// ADR 0036 §4: the parser of one answer. Its state is fixed in size, apart
/// from one bit per input of the request; it never grows with the answer.
pub(crate) struct AnswerParser {
    expected: Expected,
    /// The public name as a JSON string, written in place of the engine's.
    model: Vec<u8>,
    state: State,
    stack: [Frame; DEPTH_LIMIT],
    depth: usize,
    /// The current member name as decoded, its first [`KNOWN_NAME`] bytes.
    name: [u8; KNOWN_NAME],
    name_len: usize,
    /// The current member name's length as sent.
    name_raw: usize,
    /// The member name read last, as the slot of its value.
    pending: Slot,
    number: [u8; NUMBER_LIMIT],
    number_len: usize,
    /// UTF-8: continuation bytes still owed and the next one's range.
    utf8_need: u8,
    utf8_lo: u8,
    utf8_hi: u8,
    /// The top-level `object` value's first bytes; one more than fits marks
    /// a longer value.
    word: [u8; 8],
    word_len: usize,
    list: bool,
    base64_chars: usize,
    base64_pad: u8,
    top: u8,
    entry: u8,
    usage: u8,
    /// One bit per input: the indexes seen.
    seen: Vec<u64>,
    entries: usize,
    /// The first vector's length, in values.
    width: Option<usize>,
    /// The current `float` vector's values so far.
    components: usize,
    prompt_tokens: Option<u64>,
}

impl AnswerParser {
    /// A parser for the answer to a request that `expected` describes, which
    /// writes `public` as the answer's `model`.
    pub(crate) fn new(expected: Expected, public: &str) -> Self {
        Self {
            expected,
            model: serde_json::to_vec(public).expect("a string serializes"),
            state: State::Start,
            stack: [Frame::Top; DEPTH_LIMIT],
            depth: 0,
            name: [0; KNOWN_NAME],
            name_len: 0,
            name_raw: 0,
            pending: Slot::Other,
            number: [0; NUMBER_LIMIT],
            number_len: 0,
            utf8_need: 0,
            utf8_lo: 0x80,
            utf8_hi: 0xBF,
            word: [0; 8],
            word_len: 0,
            list: false,
            base64_chars: 0,
            base64_pad: 0,
            top: 0,
            entry: 0,
            usage: 0,
            seen: vec![0; expected.inputs.div_ceil(64)],
            entries: 0,
            width: None,
            components: 0,
            prompt_tokens: None,
        }
    }

    /// Read the next bytes of the answer, appending to `out` what is to be
    /// relayed for them: the same bytes, with the engine's `model` value
    /// replaced by the public name, and without the top-level closing brace
    /// or anything after it. On `Err`, `out` holds what is relayed of the
    /// bytes before the break, and nothing more may be relayed.
    pub(crate) fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) -> Result<(), Malformed> {
        for &byte in bytes {
            let before = out.len();
            if let Err(malformed) = self.byte(byte, out) {
                out.truncate(before);
                return Err(malformed);
            }
        }
        Ok(())
    }

    /// The answer ended: `Ok` only when it ended whole with every check
    /// passed.
    pub(crate) fn finish(&self) -> Result<Complete, Malformed> {
        if self.state != State::Done {
            return Err(Malformed("the answer ended before its end"));
        }
        Ok(Complete {
            prompt_tokens: self.prompt_tokens,
        })
    }

    fn byte(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), Malformed> {
        match self.state {
            State::Text { text, escape } => self.text(byte, text, escape, out),
            State::Number { slot, at } => {
                if self.number_byte(byte, slot, at)? {
                    out.push(byte);
                    Ok(())
                } else {
                    self.end_number(slot, at)?;
                    self.structural(byte, out)
                }
            }
            State::Literal { word, at } => {
                if word[at] != byte {
                    return Err(Malformed("invalid JSON"));
                }
                out.push(byte);
                self.state = if at + 1 == word.len() {
                    State::After
                } else {
                    State::Literal { word, at: at + 1 }
                };
                Ok(())
            }
            _ => self.structural(byte, out),
        }
    }

    /// A byte outside any string, number or literal.
    fn structural(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), Malformed> {
        let space = matches!(byte, b' ' | b'\t' | b'\n' | b'\r');
        match self.state {
            State::Done if space => Ok(()),
            State::Done => Err(Malformed("bytes after the answer's end")),
            _ if space => {
                out.push(byte);
                Ok(())
            }
            State::Start if byte == b'{' => {
                self.push(Frame::Top)?;
                self.state = State::Name { first: true };
                out.push(byte);
                Ok(())
            }
            State::Start => Err(Malformed("the answer is not a JSON object")),
            State::Name { .. } if byte == b'"' => {
                self.name_len = 0;
                self.name_raw = 0;
                self.state = State::Text {
                    text: Text::Name,
                    escape: Escape::None,
                };
                out.push(byte);
                Ok(())
            }
            State::Name { first: true } if byte == b'}' => self.close(byte, out),
            State::Name { .. } => Err(Malformed("invalid JSON: a member name was expected")),
            State::Colon if byte == b':' => {
                self.state = State::Value {
                    slot: self.pending,
                    first: false,
                };
                out.push(byte);
                Ok(())
            }
            State::Colon => Err(Malformed("invalid JSON: a colon was expected")),
            State::Value { slot, first } => self.value(byte, slot, first, out),
            State::After => match byte {
                b',' => {
                    let frame = self.stack[self.depth - 1];
                    self.state = if frame.is_object() {
                        State::Name { first: false }
                    } else {
                        State::Value {
                            slot: frame.item(),
                            first: false,
                        }
                    };
                    out.push(byte);
                    Ok(())
                }
                b'}' | b']' => self.close(byte, out),
                _ => Err(Malformed(
                    "invalid JSON: a comma or a container's end was expected",
                )),
            },
            State::Text { .. } | State::Number { .. } | State::Literal { .. } => {
                unreachable!("strings, numbers and literals are read before structure")
            }
        }
    }

    /// The first byte of a value in `slot`.
    fn value(
        &mut self,
        byte: u8,
        slot: Slot,
        first: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), Malformed> {
        match byte {
            b'{' => {
                let frame = match slot {
                    Slot::TopUsage => Frame::Usage,
                    Slot::Entry => {
                        self.entry = 0;
                        Frame::Entry
                    }
                    Slot::Other => Frame::Object,
                    _ => return Err(Malformed("an object where the shape has none")),
                };
                self.push(frame)?;
                self.state = State::Name { first: true };
            }
            b'[' => {
                let frame = match slot {
                    Slot::TopData => Frame::Data,
                    Slot::EntryEmbedding if self.expected.encoding == Encoding::Float => {
                        self.components = 0;
                        Frame::Vector
                    }
                    Slot::Other => Frame::Array,
                    _ => return Err(Malformed("an array where the shape has none")),
                };
                self.push(frame)?;
                self.state = State::Value {
                    slot: frame.item(),
                    first: true,
                };
            }
            b'"' => {
                match slot {
                    Slot::TopObject => self.word_len = 0,
                    Slot::TopModel => {
                        // ADR 0036 §4: the engine's value is dropped as it
                        // passes and the public name written in its place.
                        out.extend_from_slice(&self.model);
                        self.state = State::Text {
                            text: Text::Value(slot),
                            escape: Escape::None,
                        };
                        return Ok(());
                    }
                    Slot::EntryEmbedding if self.expected.encoding == Encoding::Base64 => {
                        self.base64_chars = 0;
                        self.base64_pad = 0;
                    }
                    Slot::EntryObject | Slot::Other => {}
                    _ => return Err(Malformed("a string where the shape has none")),
                }
                self.state = State::Text {
                    text: Text::Value(slot),
                    escape: Escape::None,
                };
            }
            b'-' | b'0'..=b'9' => {
                if !matches!(
                    slot,
                    Slot::EntryIndex
                        | Slot::PromptTokens
                        | Slot::TotalTokens
                        | Slot::Component
                        | Slot::Other
                ) {
                    return Err(Malformed("a number where the shape has none"));
                }
                self.number[0] = byte;
                self.number_len = 1;
                let at = match byte {
                    b'-' => Num::Minus,
                    b'0' => Num::Zero,
                    _ => Num::Int,
                };
                self.state = State::Number { slot, at };
            }
            b't' | b'f' | b'n' if slot == Slot::Other => {
                let word: &'static [u8] = match byte {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                };
                self.state = State::Literal { word, at: 1 };
            }
            b']' if first => return self.close(byte, out),
            _ => return Err(Malformed("invalid JSON: a value was expected")),
        }
        out.push(byte);
        Ok(())
    }

    fn push(&mut self, frame: Frame) -> Result<(), Malformed> {
        if self.depth == DEPTH_LIMIT {
            return Err(Malformed("nesting deeper than 32 levels"));
        }
        self.stack[self.depth] = frame;
        self.depth += 1;
        Ok(())
    }

    /// `}` or `]` closing the current container, with its checks.
    fn close(&mut self, byte: u8, out: &mut Vec<u8>) -> Result<(), Malformed> {
        let frame = self.stack[self.depth - 1];
        if (byte == b'}') != frame.is_object() {
            return Err(Malformed("invalid JSON: a mismatched bracket"));
        }
        match frame {
            Frame::Top => {
                if self.top & (TOP_OBJECT | TOP_DATA | TOP_MODEL)
                    != TOP_OBJECT | TOP_DATA | TOP_MODEL
                {
                    return Err(Malformed("the answer lacks object, data or model"));
                }
                if !self.list {
                    return Err(Malformed("the answer's object is not \"list\""));
                }
            }
            Frame::Data => {
                if self.entries != self.expected.inputs {
                    return Err(Malformed("data does not hold one entry per input"));
                }
            }
            Frame::Entry => {
                if self.entry != ENTRY_OBJECT | ENTRY_INDEX | ENTRY_EMBEDDING {
                    return Err(Malformed("an entry lacks object, index or embedding"));
                }
                self.entries += 1;
            }
            Frame::Vector => self.vector(self.components)?,
            Frame::Usage | Frame::Object | Frame::Array => {}
        }
        self.depth -= 1;
        if frame == Frame::Top {
            // ADR 0036 §4: the closing brace is withheld; the caller writes
            // it after `capyctl.metrics`.
            self.state = State::Done;
        } else {
            out.push(byte);
            self.state = State::After;
        }
        Ok(())
    }

    /// One whole vector of `length` values: never empty, of the asked
    /// `dimensions`, and of the first vector's length.
    fn vector(&mut self, length: usize) -> Result<(), Malformed> {
        if length == 0 {
            return Err(Malformed("an empty embedding"));
        }
        if self
            .expected
            .dimensions
            .is_some_and(|dimensions| dimensions != length)
        {
            return Err(Malformed("an embedding not of the dimensions asked"));
        }
        match self.width {
            None => self.width = Some(length),
            Some(width) if width != length => {
                return Err(Malformed("embeddings of different lengths"))
            }
            Some(_) => {}
        }
        Ok(())
    }

    /// The slot of the value of the member name just read, at the current
    /// container. A repeated member of the shape has no one interpretation.
    fn member(&mut self) -> Result<Slot, Malformed> {
        // A name the escapes spell as a known one is that one: `model`
        // written with `\u` escapes is still replaced.
        let name = if self.name_len <= KNOWN_NAME {
            &self.name[..self.name_len]
        } else {
            &[][..]
        };
        let (seen, bit, slot) = match self.stack[self.depth - 1] {
            Frame::Top => match name {
                b"object" => (&mut self.top, TOP_OBJECT, Slot::TopObject),
                b"data" => (&mut self.top, TOP_DATA, Slot::TopData),
                b"model" => (&mut self.top, TOP_MODEL, Slot::TopModel),
                b"usage" => (&mut self.top, TOP_USAGE, Slot::TopUsage),
                _ => return Ok(Slot::Other),
            },
            Frame::Entry => match name {
                b"object" => (&mut self.entry, ENTRY_OBJECT, Slot::EntryObject),
                b"index" => (&mut self.entry, ENTRY_INDEX, Slot::EntryIndex),
                b"embedding" => (&mut self.entry, ENTRY_EMBEDDING, Slot::EntryEmbedding),
                _ => return Ok(Slot::Other),
            },
            Frame::Usage => match name {
                b"prompt_tokens" => (&mut self.usage, USAGE_PROMPT, Slot::PromptTokens),
                b"total_tokens" => (&mut self.usage, USAGE_TOTAL, Slot::TotalTokens),
                _ => return Ok(Slot::Other),
            },
            _ => return Ok(Slot::Other),
        };
        if *seen & bit != 0 {
            return Err(Malformed("a repeated member"));
        }
        *seen |= bit;
        Ok(slot)
    }

    /// One byte inside a string (a member name or a value).
    fn text(
        &mut self,
        byte: u8,
        text: Text,
        escape: Escape,
        out: &mut Vec<u8>,
    ) -> Result<(), Malformed> {
        if text != Text::Value(Slot::TopModel) {
            out.push(byte);
        }
        let closing = byte == b'"' && escape == Escape::None && self.utf8_need == 0;
        if text == Text::Name && !closing {
            self.name_raw += 1;
            if self.name_raw > NAME_LIMIT {
                return Err(Malformed("a member name longer than 256 bytes"));
            }
        }
        let escape = match escape {
            Escape::None if self.utf8_need > 0 => {
                if byte < self.utf8_lo || byte > self.utf8_hi {
                    return Err(Malformed("invalid UTF-8"));
                }
                self.utf8_need -= 1;
                self.utf8_lo = 0x80;
                self.utf8_hi = 0xBF;
                self.decoded(text, byte)?;
                Escape::None
            }
            Escape::None => {
                let (need, lo, hi) = match byte {
                    b'"' => return self.end_text(text),
                    b'\\' => {
                        self.state = State::Text {
                            text,
                            escape: Escape::Backslash,
                        };
                        return Ok(());
                    }
                    0x00..=0x1F => return Err(Malformed("a control character in a string")),
                    0x20..=0x7F => (0, 0x80, 0xBF),
                    0xC2..=0xDF => (1, 0x80, 0xBF),
                    0xE0 => (2, 0xA0, 0xBF),
                    0xE1..=0xEC | 0xEE..=0xEF => (2, 0x80, 0xBF),
                    0xED => (2, 0x80, 0x9F),
                    0xF0 => (3, 0x90, 0xBF),
                    0xF1..=0xF3 => (3, 0x80, 0xBF),
                    0xF4 => (3, 0x80, 0x8F),
                    _ => return Err(Malformed("invalid UTF-8")),
                };
                self.utf8_need = need;
                self.utf8_lo = lo;
                self.utf8_hi = hi;
                self.decoded(text, byte)?;
                Escape::None
            }
            Escape::Backslash => {
                let decoded = match byte {
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'/' => b'/',
                    b'b' => 0x08,
                    b'f' => 0x0C,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'u' => {
                        self.state = State::Text {
                            text,
                            escape: Escape::Hex {
                                digits: 0,
                                value: 0,
                                high: None,
                            },
                        };
                        return Ok(());
                    }
                    _ => return Err(Malformed("invalid JSON: an unknown escape")),
                };
                self.decoded(text, decoded)?;
                Escape::None
            }
            Escape::Hex {
                digits,
                value,
                high,
            } => {
                let digit = (byte as char).to_digit(16).ok_or(Malformed(
                    "invalid JSON: a \\u escape without four hex digits",
                ))?;
                let value = value * 16 + digit;
                if digits < 3 {
                    Escape::Hex {
                        digits: digits + 1,
                        value,
                        high,
                    }
                } else {
                    let point = match (high, value) {
                        (None, 0xD800..=0xDBFF) => {
                            self.state = State::Text {
                                text,
                                escape: Escape::WantLow { high: value },
                            };
                            return Ok(());
                        }
                        (Some(high), 0xDC00..=0xDFFF) => {
                            0x10000 + ((high - 0xD800) << 10) + (value - 0xDC00)
                        }
                        (None, 0xDC00..=0xDFFF) | (Some(_), _) => {
                            return Err(Malformed("invalid JSON: a lone surrogate"))
                        }
                        (None, value) => value,
                    };
                    let point =
                        char::from_u32(point).ok_or(Malformed("invalid JSON: a lone surrogate"))?;
                    let mut utf8 = [0; 4];
                    for &byte in point.encode_utf8(&mut utf8).as_bytes() {
                        self.decoded(text, byte)?;
                    }
                    Escape::None
                }
            }
            Escape::WantLow { high } if byte == b'\\' => Escape::WantLowU { high },
            Escape::WantLowU { high } if byte == b'u' => Escape::Hex {
                digits: 0,
                value: 0,
                high: Some(high),
            },
            Escape::WantLow { .. } | Escape::WantLowU { .. } => {
                return Err(Malformed("invalid JSON: a lone surrogate"))
            }
        };
        self.state = State::Text { text, escape };
        Ok(())
    }

    /// One byte of a string's decoded content.
    fn decoded(&mut self, text: Text, byte: u8) -> Result<(), Malformed> {
        match text {
            Text::Name => {
                if self.name_len < KNOWN_NAME {
                    self.name[self.name_len] = byte;
                }
                self.name_len = (self.name_len + 1).min(KNOWN_NAME + 1);
            }
            Text::Value(Slot::TopObject) => {
                if self.word_len < self.word.len() {
                    self.word[self.word_len] = byte;
                }
                self.word_len = (self.word_len + 1).min(self.word.len() + 1);
            }
            Text::Value(Slot::EntryEmbedding) => match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' if self.base64_pad == 0 => {
                    self.base64_chars += 1;
                }
                b'=' if self.base64_pad < 2 => {
                    self.base64_pad += 1;
                    self.base64_chars += 1;
                }
                _ => return Err(Malformed("an embedding that is not base64")),
            },
            Text::Value(_) => {}
        }
        Ok(())
    }

    /// A string's closing quote.
    fn end_text(&mut self, text: Text) -> Result<(), Malformed> {
        match text {
            Text::Name => {
                self.pending = self.member()?;
                self.state = State::Colon;
                return Ok(());
            }
            Text::Value(Slot::TopObject) => {
                self.list =
                    self.word_len <= self.word.len() && &self.word[..self.word_len] == b"list";
            }
            Text::Value(Slot::EntryEmbedding) => {
                // ADR 0036 §4: base64 of a whole number of 4-byte values.
                let chars = self.base64_chars;
                if !chars.is_multiple_of(4) {
                    return Err(Malformed("an embedding that is not base64"));
                }
                let bytes = chars / 4 * 3 - usize::from(self.base64_pad);
                if !bytes.is_multiple_of(4) {
                    return Err(Malformed("a base64 embedding not of whole 4-byte values"));
                }
                self.vector(bytes / 4)?;
            }
            Text::Value(_) => {}
        }
        self.state = State::After;
        Ok(())
    }

    /// Whether `byte` continues the number; `Err` when the number would be
    /// longer than [`NUMBER_LIMIT`].
    fn number_byte(&mut self, byte: u8, slot: Slot, at: Num) -> Result<bool, Malformed> {
        let next = match (at, byte) {
            (Num::Minus, b'0') => Num::Zero,
            (Num::Minus | Num::Int, b'0'..=b'9') => Num::Int,
            (Num::Zero | Num::Int, b'.') => Num::Dot,
            (Num::Dot | Num::Frac, b'0'..=b'9') => Num::Frac,
            (Num::Zero | Num::Int | Num::Frac, b'e' | b'E') => Num::Exp,
            (Num::Exp, b'+' | b'-') => Num::ExpSign,
            (Num::Exp | Num::ExpSign | Num::ExpDigits, b'0'..=b'9') => Num::ExpDigits,
            _ => return Ok(false),
        };
        if self.number_len == NUMBER_LIMIT {
            return Err(Malformed("a number longer than 64 bytes"));
        }
        self.number[self.number_len] = byte;
        self.number_len += 1;
        self.state = State::Number { slot, at: next };
        Ok(true)
    }

    /// The number just read, checked for its slot.
    fn end_number(&mut self, slot: Slot, at: Num) -> Result<(), Malformed> {
        if !matches!(at, Num::Zero | Num::Int | Num::Frac | Num::ExpDigits) {
            return Err(Malformed("invalid JSON: an incomplete number"));
        }
        let text = std::str::from_utf8(&self.number[..self.number_len])
            .expect("a number's bytes are ASCII");
        let count = || -> Result<u64, Malformed> {
            if !matches!(at, Num::Zero | Num::Int) || text.starts_with('-') {
                return Err(Malformed("a count that is not a non-negative integer"));
            }
            text.parse()
                .map_err(|_| Malformed("a count that is not a non-negative integer"))
        };
        match slot {
            Slot::EntryIndex => {
                let index = count()?;
                let index = usize::try_from(index)
                    .ok()
                    .filter(|index| *index < self.expected.inputs)
                    .ok_or(Malformed("an index outside the inputs"))?;
                let (word, bit) = (index / 64, 1u64 << (index % 64));
                if self.seen[word] & bit != 0 {
                    return Err(Malformed("a repeated index"));
                }
                self.seen[word] |= bit;
            }
            Slot::PromptTokens => self.prompt_tokens = Some(count()?),
            Slot::TotalTokens => {
                count()?;
            }
            Slot::Component => {
                // A vector holds float32 values: a value beyond float32's
                // range is not finite.
                let value: f64 = text.parse().map_err(|_| Malformed("invalid JSON"))?;
                if !(value as f32).is_finite() {
                    return Err(Malformed("a non-finite value"));
                }
                self.components += 1;
            }
            _ => {}
        }
        self.state = State::After;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{AnswerParser, Complete, Encoding, Expected, Malformed};

    fn float(inputs: usize) -> Expected {
        Expected {
            inputs,
            encoding: Encoding::Float,
            dimensions: None,
        }
    }

    /// Feed `answer` in chunks of `chunk` bytes; what was relayed and how the
    /// answer ended.
    fn run(
        expected: Expected,
        answer: &str,
        chunk: usize,
    ) -> (String, Result<Complete, Malformed>) {
        let mut parser = AnswerParser::new(expected, "public/name");
        let mut out = Vec::new();
        for part in answer.as_bytes().chunks(chunk) {
            if let Err(malformed) = parser.feed(part, &mut out) {
                return (String::from_utf8(out).unwrap(), Err(malformed));
            }
        }
        (String::from_utf8(out).unwrap(), parser.finish())
    }

    fn answer(model_first: bool, data: &str) -> String {
        if model_first {
            format!(
                r#"{{"id":"embd-1","object":"list","created":7,"model":"/models/Qwen \"8B\"","data":{data},"usage":{{"prompt_tokens":5,"total_tokens":5,"completion_tokens":0}}}}"#
            )
        } else {
            format!(
                r#"{{"object":"list","data":{data},"model":"/models/Qwen \"8B\"","usage":{{"prompt_tokens":5,"total_tokens":5}}}}"#
            )
        }
    }

    const TWO: &str = r#"[{"object":"embedding","index":1,"embedding":[0.5,-1e-3,0],"encoding_format":"float"},{"index":0,"object":"embedding","embedding":[1,2.25E+2,-0.0]}]"#;

    /// ADR 0036 §3-§4: `model` is the public name whether it comes before or
    /// after `data`, and split across chunks at every byte; nothing else is
    /// rewritten, and the closing brace is withheld.
    // T47
    #[test]
    fn model_is_rewritten_before_and_after_data_at_every_split_and_nothing_else_is() {
        for model_first in [true, false] {
            let original = answer(model_first, TWO);
            let expected = original.replace(r#""/models/Qwen \"8B\"""#, r#""public/name""#);
            let expected = expected.strip_suffix('}').unwrap();
            for chunk in 1..=original.len() {
                let (out, end) = run(float(2), &original, chunk);
                assert_eq!(
                    end,
                    Ok(Complete {
                        prompt_tokens: Some(5)
                    }),
                    "{chunk}"
                );
                assert_eq!(out, expected, "split every {chunk} bytes");
            }
            // Whitespace after the top-level object is withheld too.
            let (out, end) = run(float(2), &format!("{original} \r\n\t"), 7);
            assert!(end.is_ok());
            assert_eq!(out, expected);
            let whole = format!("{out}}}");
            let parsed: serde_json::Value = serde_json::from_str(&whole).unwrap();
            assert_eq!(parsed["model"], "public/name");
        }
    }

    /// ADR 0036 §4: base64 vectors decode to a whole number of 4-byte values,
    /// the same number for every entry and of the asked `dimensions`.
    // T47
    #[test]
    fn base64_vectors_are_whole_float32_values_of_one_length() {
        let base64 = |dimensions| Expected {
            inputs: 2,
            encoding: Encoding::Base64,
            dimensions,
        };
        let data = |first: &str, second: &str| {
            answer(
                false,
                &format!(
                    r#"[{{"object":"embedding","index":0,"embedding":"{first}"}},{{"object":"embedding","index":1,"embedding":"{second}"}}]"#
                ),
            )
        };
        for (first, second) in [("AACAPw==", "AAAAQA=="), ("AAAAAAAAAAA=", "AAAAAAAAAAA=")] {
            assert!(run(base64(None), &data(first, second), 3).1.is_ok());
        }
        assert!(
            run(base64(Some(2)), &data("AAAAAAAAAAA=", "AAAAAAAAAAA="), 3)
                .1
                .is_ok()
        );
        for (first, second, dimensions) in [
            // Unequal lengths.
            ("AACAPw==", "AAAAAAAAAAA=", None),
            // Nine bytes, not whole 4-byte values.
            ("AAAAAAAAAAAA", "AAAAAAAAAAAA", None),
            // Not base64.
            ("AACAPw=A", "AAAAQA==", None),
            ("AAC*Pw==", "AAAAQA==", None),
            ("AACAP", "AAAAQA==", None),
            ("", "", None),
            // Not of the dimensions asked.
            ("AACAPw==", "AAAAQA==", Some(2)),
        ] {
            assert!(
                run(base64(dimensions), &data(first, second), 3).1.is_err(),
                "{first} {second}"
            );
        }
        // A float vector where base64 was asked, and the reverse.
        assert!(run(base64(None), &answer(false, TWO), 3).1.is_err());
        assert!(run(float(2), &data("AACAPw==", "AAAAQA=="), 3).1.is_err());
    }

    /// ADR 0036 §4: an answer that breaks a check stops: nothing after the
    /// break is relayed and the closing brace never is.
    // T47
    #[test]
    fn a_broken_answer_is_malformed_and_never_relayed_whole() {
        let entry = |index: usize, vector: &str| {
            format!(r#"{{"object":"embedding","index":{index},"embedding":{vector}}}"#)
        };
        let data = |entries: &[String]| format!("[{}]", entries.join(","));
        let long_number = format!("0.{}", "1".repeat(63));
        let long_name = "n".repeat(257);
        let deep = format!("{}1{}", "[".repeat(32), "]".repeat(32));
        let cases: Vec<(&str, String, Expected)> = vec![
            (
                "too few entries",
                answer(false, &data(&[entry(0, "[1]")])),
                float(2),
            ),
            (
                "too many entries",
                answer(
                    false,
                    &data(&[entry(0, "[1]"), entry(1, "[1]"), entry(2, "[1]")]),
                ),
                float(2),
            ),
            (
                "a repeated index",
                answer(false, &data(&[entry(0, "[1]"), entry(0, "[1]")])),
                float(2),
            ),
            (
                "ragged vectors",
                answer(false, &data(&[entry(0, "[1,2]"), entry(1, "[1]")])),
                float(2),
            ),
            (
                "an empty vector",
                answer(false, &data(&[entry(0, "[]")])),
                float(1),
            ),
            (
                "a non-finite value",
                answer(false, &data(&[entry(0, "[1e39]")])),
                float(1),
            ),
            (
                "the wrong dimensions",
                answer(false, &data(&[entry(0, "[1,2,3]")])),
                Expected {
                    dimensions: Some(2),
                    ..float(1)
                },
            ),
            (
                "an oversized number",
                answer(false, &data(&[entry(0, &format!("[{long_number}]"))])),
                float(1),
            ),
            (
                "an oversized member name",
                answer(false, &data(&[entry(0, "[1]")])).replacen(
                    "{",
                    &format!(r#"{{"{long_name}":1,"#),
                    1,
                ),
                float(1),
            ),
            (
                "nesting deeper than 32 levels",
                answer(false, &data(&[entry(0, "[1]")])).replacen(
                    "{",
                    &format!(r#"{{"x":{deep},"#),
                    1,
                ),
                float(1),
            ),
            (
                "a fractional index",
                answer(
                    false,
                    &data(&[entry(0, "[1]")]).replace(r#""index":0"#, r#""index":0.0"#),
                ),
                float(1),
            ),
            (
                "a negative index",
                answer(
                    false,
                    &data(&[entry(0, "[1]")]).replace(r#""index":0"#, r#""index":-1"#),
                ),
                float(1),
            ),
            (
                "negative usage",
                answer(false, &data(&[entry(0, "[1]")])).replace(":5,", ":-5,"),
                float(1),
            ),
            (
                "not a list",
                answer(false, &data(&[entry(0, "[1]")])).replace(r#""list""#, r#""lists""#),
                float(1),
            ),
            (
                "no model",
                answer(false, &data(&[entry(0, "[1]")]))
                    .replace(r#""model":"/models/Qwen \"8B\"","#, ""),
                float(1),
            ),
            (
                "a repeated model",
                answer(false, &data(&[entry(0, "[1]")])).replacen("{", r#"{"model":"m","#, 1),
                float(1),
            ),
            (
                "an entry without its index",
                answer(
                    false,
                    &data(&[r#"{"object":"embedding","embedding":[1]}"#.into()]),
                ),
                float(1),
            ),
            ("NaN", answer(false, &data(&[entry(0, "[NaN]")])), float(1)),
            (
                "a trailing comma",
                answer(false, &data(&[entry(0, "[1,]")])),
                float(1),
            ),
            (
                "a leading zero",
                answer(false, &data(&[entry(0, "[01]")])),
                float(1),
            ),
            (
                "a bad escape",
                answer(false, &data(&[entry(0, "[1]")])).replace(r#"\"8B"#, r#"\q8B"#),
                float(1),
            ),
            (
                "a lone surrogate",
                answer(false, &data(&[entry(0, "[1]")])).replace(r#"\"8B"#, r#"\ud800B"#),
                float(1),
            ),
            (
                "bytes after the end",
                answer(false, &data(&[entry(0, "[1]")])) + "{}",
                float(1),
            ),
            (
                "a mismatched bracket",
                answer(false, &data(&[entry(0, "[1}")])),
                float(1),
            ),
            ("not an object", "[]".into(), float(1)),
        ];
        for (what, answer, expected) in cases {
            let (out, end) = run(expected, &answer, 5);
            assert!(end.is_err(), "{what} must be malformed");
            assert!(
                serde_json::from_str::<serde_json::Value>(&out).is_err(),
                "{what}"
            );
        }
        // Invalid UTF-8 in a string.
        let mut bytes = answer(false, &data(&[entry(0, "[1]")])).into_bytes();
        let at = bytes.iter().position(|byte| *byte == b'Q').unwrap();
        bytes[at] = 0xFF;
        let mut parser = AnswerParser::new(float(1), "p");
        assert!(parser.feed(&bytes, &mut Vec::new()).is_err());
        // An answer cut short relays a prefix and is not complete.
        let whole = answer(true, TWO);
        for end in 0..whole.len() {
            let (out, finished) = run(float(2), &whole[..end], 4);
            assert!(finished.is_err(), "cut at {end}");
            assert!(serde_json::from_str::<serde_json::Value>(&out).is_err());
        }
    }

    /// ADR 0036 §4: a number of 64 bytes, a name of 256 bytes and 32 levels
    /// are read; members the shape does not name are relayed as any value.
    // T47
    #[test]
    fn the_bounds_are_inclusive_and_other_members_are_relayed() {
        let number = format!("0.{}", "1".repeat(62));
        let name = "n".repeat(256);
        let deep = format!("{}1{}", "[".repeat(31), "]".repeat(31));
        let original = answer(
            false,
            &format!(
                r#"[{{"object":"embedding","index":0,"embedding":[{number}],"extra":{{"a":[true,false,null,"\u00e9\ud83d\ude00/"]}}}}]"#
            ),
        )
        .replacen("{", &format!(r#"{{"{name}":{deep},"#), 1);
        let (out, end) = run(float(1), &original, 9);
        assert_eq!(
            end,
            Ok(Complete {
                prompt_tokens: Some(5)
            })
        );
        let relayed: serde_json::Value = serde_json::from_str(&format!("{out}}}")).unwrap();
        let sent: serde_json::Value = serde_json::from_str(&original).unwrap();
        assert_eq!(relayed[name.as_str()], sent[name.as_str()]);
        assert_eq!(relayed["data"], sent["data"]);
        // No usage at all is no prompt count.
        let bare =
            r#"{"object":"list","data":[{"object":"e","index":0,"embedding":[1]}],"model":"m"}"#;
        assert_eq!(
            run(float(1), bare, 2).1,
            Ok(Complete {
                prompt_tokens: None
            })
        );
        // `model` spelled with an escape is still the engine's model.
        let escaped = r#"{"object":"list","data":[{"object":"e","index":0,"embedding":[1]}],"mod\u0065l":"engine"}"#;
        let (out, end) = run(float(1), escaped, 3);
        assert!(end.is_ok());
        assert!(out.ends_with(r#""mod\u0065l":"public/name""#), "{out}");
    }

    /// ADR 0036 §4: the parser's state is fixed in size; with the bitset of
    /// a 2048-input request it stays a few hundred bytes, whatever the answer.
    // T47
    #[test]
    fn the_parser_state_is_bounded() {
        assert!(std::mem::size_of::<AnswerParser>() <= 512);
        let parser = AnswerParser::new(float(2048), "public");
        assert_eq!(parser.seen.len(), 32);
    }
}
