//! The text of a PDF, for `synck diff`.
//!
//! A PDF draws text as positioned glyphs in compressed content streams, so one
//! changed word changes bytes no reader can compare. This recovers the text
//! each page draws and reduces it to lines as the Office reader does: `Page N`,
//! then one indented line per paragraph.
//!
//! A glyph becomes text through its font's ToUnicode map, or else through the
//! font's encoding and glyph names. Glyphs on one baseline form a line, in the
//! order they are drawn, with a space where the gap between two is wider than
//! a fraction of the font size. Consecutive lines join into one paragraph when
//! the next line's first word would not have fit at the end of the previous
//! line: that is where a layout engine wraps. A word added to a paragraph then
//! changes one line of the diff, not every line the paragraph rewraps.
//!
//! The text does not have to match what a viewer copies. It has to come out
//! the same for the same drawing, so that two versions differ only where
//! their text does. Annotations and form fields are left out.
//!
//! Objects are found by scanning for `N G obj`, not through the
//! cross-reference table, and a later definition replaces an earlier one, as
//! an appended update does. An encrypted file, a filter other than Flate, and
//! anything else this cannot read gives no text, and `diff` reports a binary
//! file.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

/// Most bytes decompressed from one document.
const MAX_INFLATED: usize = 64 * 1024 * 1024;
/// Most content bytes interpreted for one document, counting a form again
/// each time it is drawn.
const MAX_CONTENT: usize = 4 * MAX_INFLATED;
/// Deepest nesting of objects, of the page tree, of forms and of saved
/// graphics states.
const MAX_DEPTH: usize = 64;
/// Most operands before an operator. No operator takes as many.
const MAX_OPERANDS: usize = 64;
/// A gap between glyphs on a line wider than this many font sizes is a space.
const WORD_GAP: f64 = 0.15;
/// The width of a space, in font sizes, when deciding whether a word would
/// have fit at the end of a line.
const SPACE: f64 = 0.25;
/// A line can continue any of this many blocks begun last on its page, so a
/// column of line numbers drawn between a paragraph's lines does not split it.
const OPEN_BLOCKS: usize = 8;

pub(crate) fn document_text(bytes: &[u8]) -> Option<String> {
    // The header may follow other bytes within the first 1024.
    find(&bytes[..bytes.len().min(1024)], b"%PDF-", 0)?;
    let document = Document::read(bytes)?;
    let mut page = Page {
        document: &document,
        fonts: HashMap::new(),
        content: 0,
        line: None,
        lines: Vec::new(),
    };
    let mut pages = Vec::new();
    for (dict, resources) in document.pages()? {
        page.run(&document.contents(dict)?, resources, IDENTITY, 0)?;
        page.end_line();
        pages.push(std::mem::take(&mut page.lines));
    }
    let spacing = line_spacing(&pages);
    let mut out = String::new();
    for (number, lines) in pages.iter().enumerate() {
        out.push_str(&format!("Page {}\n", number + 1));
        paragraphs(lines, &spacing, &mut out);
    }
    Some(out)
}

#[derive(Clone, Debug, PartialEq)]
enum Object {
    /// null, true and false, which nothing here reads.
    Other,
    Number(f64),
    String(Vec<u8>),
    Name(Vec<u8>),
    Array(Vec<Object>),
    Dict(Dict),
    /// A stream's dictionary and where its data is in the file.
    Stream(Dict, Range<usize>),
    Ref(u32),
}

static OTHER: Object = Object::Other;

/// Entries in file order; a repeated key means its last value.
#[derive(Clone, Debug, PartialEq)]
struct Dict(Vec<(Vec<u8>, Object)>);

impl Dict {
    fn get(&self, key: &[u8]) -> Option<&Object> {
        self.0.iter().rev().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    fn is(&self, key: &[u8], name: &[u8]) -> bool {
        matches!(self.get(key), Some(Object::Name(value)) if value == name)
    }
}

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Object(Object),
    Keyword(&'a [u8]),
    ArrayStart,
    ArrayEnd,
    DictStart,
    DictEnd,
}

struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
    /// Whether `N G R` is a reference: in objects, not in content streams.
    refs: bool,
}

fn is_space(byte: u8) -> bool {
    matches!(byte, b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn is_delimiter(byte: u8) -> bool {
    matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

fn is_regular(byte: u8) -> bool {
    !is_space(byte) && !is_delimiter(byte)
}

/// A PDF number: digits with at most one point and an optional sign.
fn number(word: &[u8]) -> Option<f64> {
    let digits = word.strip_prefix(b"-").or_else(|| word.strip_prefix(b"+")).unwrap_or(word);
    let valid = digits.iter().any(u8::is_ascii_digit)
        && digits.iter().all(|byte| byte.is_ascii_digit() || *byte == b'.')
        && digits.iter().filter(|byte| **byte == b'.').count() <= 1;
    if !valid {
        return None;
    }
    std::str::from_utf8(word).ok()?.parse().ok()
}

fn whole(number: f64) -> Option<u32> {
    (number >= 0.0 && number <= f64::from(u32::MAX) && number.fract() == 0.0).then_some(number as u32)
}

impl<'a> Lexer<'a> {
    fn new(data: &'a [u8], pos: usize, refs: bool) -> Self {
        Self { data, pos, refs }
    }

    fn peek(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn skip_space(&mut self) {
        while let Some(byte) = self.peek() {
            if byte == b'%' {
                while self.peek().is_some_and(|byte| byte != b'\r' && byte != b'\n') {
                    self.pos += 1;
                }
            } else if is_space(byte) {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn at_end(&mut self) -> bool {
        self.skip_space();
        self.pos >= self.data.len()
    }

    /// The next token; None at the end and at bytes that start none.
    fn token(&mut self) -> Option<Token<'a>> {
        self.skip_space();
        let start = self.pos;
        let byte = self.peek()?;
        self.pos += 1;
        match byte {
            b'[' => Some(Token::ArrayStart),
            b']' => Some(Token::ArrayEnd),
            b'<' if self.peek() == Some(b'<') => {
                self.pos += 1;
                Some(Token::DictStart)
            }
            b'>' if self.peek() == Some(b'>') => {
                self.pos += 1;
                Some(Token::DictEnd)
            }
            b'<' => self.hex_string().map(|bytes| Token::Object(Object::String(bytes))),
            b'(' => self.literal_string().map(|bytes| Token::Object(Object::String(bytes))),
            b'/' => Some(Token::Object(Object::Name(self.name()))),
            _ if is_delimiter(byte) => None,
            _ => {
                while self.peek().is_some_and(is_regular) {
                    self.pos += 1;
                }
                let word = &self.data[start..self.pos];
                Some(match word {
                    b"true" | b"false" | b"null" => Token::Object(Object::Other),
                    _ => number(word).map_or(Token::Keyword(word), |number| Token::Object(Object::Number(number))),
                })
            }
        }
    }

    fn literal_string(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        loop {
            let byte = self.peek()?;
            self.pos += 1;
            match byte {
                b'(' => {
                    depth += 1;
                    out.push(byte);
                }
                b')' if depth == 0 => return Some(out),
                b')' => {
                    depth -= 1;
                    out.push(byte);
                }
                // An unescaped end of line of any kind is a line feed.
                b'\r' => {
                    if self.peek() == Some(b'\n') {
                        self.pos += 1;
                    }
                    out.push(b'\n');
                }
                b'\\' => {
                    let escaped = self.peek()?;
                    self.pos += 1;
                    match escaped {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'0'..=b'7' => {
                            let mut value = u32::from(escaped - b'0');
                            for _ in 0..2 {
                                let Some(digit @ b'0'..=b'7') = self.peek() else {
                                    break;
                                };
                                value = value * 8 + u32::from(digit - b'0');
                                self.pos += 1;
                            }
                            // An octal escape above \377 keeps its low byte.
                            out.push((value & 0xff) as u8);
                        }
                        // A backslash before an end of line continues the string.
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                _ => out.push(byte),
            }
        }
    }

    fn hex_string(&mut self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut high: Option<u8> = None;
        loop {
            let byte = self.peek()?;
            self.pos += 1;
            if byte == b'>' {
                // An odd final digit is followed by an implied 0.
                out.extend(high.map(|high| high << 4));
                return Some(out);
            }
            if is_space(byte) {
                continue;
            }
            let nibble = char::from(byte).to_digit(16)? as u8;
            match high.take() {
                Some(high) => out.push(high << 4 | nibble),
                None => high = Some(nibble),
            }
        }
    }

    fn name(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(byte) = self.peek().filter(|byte| is_regular(*byte)) {
            self.pos += 1;
            let hex = self.data.get(self.pos..self.pos + 2);
            if byte == b'#'
                && let Some(hex) = hex.filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
            {
                let digits = std::str::from_utf8(hex).expect("hex digits are ASCII");
                out.push(u8::from_str_radix(digits, 16).expect("two hex digits fit a byte"));
                self.pos += 2;
                continue;
            }
            out.push(byte);
        }
        out
    }

    fn object(&mut self, depth: usize) -> Option<Object> {
        let token = self.token()?;
        self.object_from(token, depth)
    }

    fn object_from(&mut self, token: Token<'a>, depth: usize) -> Option<Object> {
        if depth > MAX_DEPTH {
            return None;
        }
        match token {
            Token::Object(Object::Number(number)) if self.refs => {
                let save = self.pos;
                if let Some(id) = whole(number)
                    && let Some(Token::Object(Object::Number(generation))) = self.token()
                    && whole(generation).is_some()
                    && self.token() == Some(Token::Keyword(b"R"))
                {
                    return Some(Object::Ref(id));
                }
                self.pos = save;
                Some(Object::Number(number))
            }
            Token::Object(object) => Some(object),
            Token::ArrayStart => {
                let mut items = Vec::new();
                loop {
                    match self.token()? {
                        Token::ArrayEnd => return Some(Object::Array(items)),
                        token => items.push(self.object_from(token, depth + 1)?),
                    }
                }
            }
            Token::DictStart => {
                let mut entries = Vec::new();
                loop {
                    match self.token()? {
                        Token::DictEnd => return Some(Object::Dict(Dict(entries))),
                        Token::Object(Object::Name(key)) => {
                            let value = self.object(depth + 1)?;
                            entries.push((key, value));
                        }
                        _ => return None,
                    }
                }
            }
            _ => None,
        }
    }

    /// The object after `N G obj`, with where its data is if it is a stream.
    fn indirect(&mut self) -> Option<Object> {
        let object = self.object(0)?;
        let Object::Dict(dict) = object else {
            return Some(object);
        };
        let save = self.pos;
        if self.token() != Some(Token::Keyword(b"stream")) {
            self.pos = save;
            return Some(Object::Dict(dict));
        }
        if self.peek() == Some(b'\r') {
            self.pos += 1;
        }
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        }
        let start = self.pos;
        // A direct /Length is trusted when `endstream` follows it; otherwise
        // the data runs to the next `endstream`, and `Document::decode`
        // tries the /Length an indirect object holds.
        let declared = match dict.get(b"Length") {
            Some(Object::Number(length)) => whole(*length).and_then(|length| start.checked_add(length as usize)),
            _ => None,
        }
        .filter(|end| ends_stream(self.data, *end));
        let end = match declared {
            Some(end) => end,
            None => {
                let mut end = find(self.data, b"endstream", start)?;
                if end > start && self.data[end - 1] == b'\n' {
                    end -= 1;
                }
                if end > start && self.data[end - 1] == b'\r' {
                    end -= 1;
                }
                end
            }
        };
        self.pos = find(self.data, b"endstream", end)? + b"endstream".len();
        Some(Object::Stream(dict, start..end))
    }
}

/// Whether `endstream` follows `at`, after any white space.
fn ends_stream(data: &[u8], at: usize) -> bool {
    at <= data.len() && Lexer::new(data, at, false).token() == Some(Token::Keyword(b"endstream"))
}

fn find(data: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

/// The object number of the `N G obj` whose `obj` starts at `at`.
fn object_number(data: &[u8], at: usize) -> Option<u32> {
    // The digits before `end`, separated from it by white space.
    let digits_before = |end: usize| -> Option<(usize, u32)> {
        let digits_end = (0..end).rev().find(|&at| !is_space(data[at]))? + 1;
        if digits_end == end {
            return None;
        }
        let start = (0..digits_end)
            .rev()
            .find(|&at| !data[at].is_ascii_digit())
            .map_or(0, |at| at + 1);
        let digits = std::str::from_utf8(&data[start..digits_end]).expect("digits are ASCII");
        Some((start, digits.parse().ok()?))
    };
    let (start, _generation) = digits_before(at)?;
    let (start, number) = digits_before(start)?;
    (start == 0 || !is_regular(data[start - 1])).then_some(number)
}

struct Document<'a> {
    data: &'a [u8],
    objects: HashMap<u32, Object>,
    root: u32,
    /// Decoded streams by object number, so a form drawn on every page is
    /// decompressed once.
    streams: RefCell<HashMap<u32, Rc<[u8]>>>,
    inflated: Cell<usize>,
}

impl<'a> Document<'a> {
    fn read(data: &'a [u8]) -> Option<Self> {
        let mut document = Document {
            data,
            objects: HashMap::new(),
            root: 0,
            streams: RefCell::new(HashMap::new()),
            inflated: Cell::new(0),
        };
        // Trailer dictionaries by position: classic trailers and
        // cross-reference streams. The last names the catalog.
        let mut trailers = Vec::new();
        let mut catalog = None;
        let mut pos = 0;
        while let Some(at) = find(data, b"obj", pos) {
            pos = at + 3;
            if data.get(pos).is_some_and(|byte| is_regular(*byte)) {
                continue;
            }
            let Some(id) = object_number(data, at) else {
                continue;
            };
            let mut lexer = Lexer::new(data, pos, true);
            let Some(object) = lexer.indirect() else {
                continue;
            };
            pos = lexer.pos;
            let mut defined = vec![(id, object)];
            if let Object::Stream(dict, range) = &defined[0].1 {
                if dict.is(b"Type", b"XRef") {
                    trailers.push((at, dict.clone()));
                }
                if dict.is(b"Type", b"ObjStm") {
                    let contained = document.object_stream(dict, range.clone());
                    defined.extend(contained);
                }
            }
            for (id, object) in defined {
                if let Object::Dict(dict) = &object
                    && dict.is(b"Type", b"Catalog")
                {
                    catalog = Some(id);
                }
                document.objects.insert(id, object);
            }
        }
        let mut pos = 0;
        while let Some(at) = find(data, b"trailer", pos) {
            pos = at + b"trailer".len();
            if let Some(Object::Dict(dict)) = Lexer::new(data, pos, true).object(0) {
                trailers.push((at, dict));
            }
        }
        trailers.sort_by_key(|(at, _)| *at);
        if trailers.iter().any(|(_, dict)| dict.get(b"Encrypt").is_some()) {
            return None;
        }
        document.root = trailers
            .iter()
            .rev()
            .find_map(|(_, dict)| match dict.get(b"Root") {
                Some(Object::Ref(id)) => Some(*id),
                _ => None,
            })
            .or(catalog)?;
        Some(document)
    }

    /// The objects an object stream holds; none if it cannot be read.
    fn object_stream(&self, dict: &Dict, range: Range<usize>) -> Vec<(u32, Object)> {
        let mut objects = Vec::new();
        let (Some(count), Some(first)) = (self.whole(dict.get(b"N")), self.whole(dict.get(b"First"))) else {
            return objects;
        };
        let Some(data) = self.decode(dict, range) else {
            return objects;
        };
        let first = first as usize;
        let mut header = Lexer::new(&data[..first.min(data.len())], 0, false);
        for _ in 0..count {
            let (Some(Token::Object(Object::Number(id))), Some(Token::Object(Object::Number(offset)))) =
                (header.token(), header.token())
            else {
                break;
            };
            let (Some(id), Some(offset)) = (whole(id), whole(offset)) else {
                break;
            };
            if let Some(object) = Lexer::new(&data, first.saturating_add(offset as usize), true).object(0) {
                objects.push((id, object));
            }
        }
        objects
    }

    /// The object a reference refers to; a missing object is null.
    fn resolve<'o>(&'o self, mut object: &'o Object) -> &'o Object {
        for _ in 0..MAX_DEPTH {
            let Object::Ref(id) = object else {
                return object;
            };
            object = self.objects.get(id).unwrap_or(&OTHER);
        }
        &OTHER
    }

    fn get<'o>(&'o self, dict: &'o Dict, key: &[u8]) -> Option<&'o Object> {
        dict.get(key).map(|object| self.resolve(object))
    }

    fn dict<'o>(&'o self, object: &'o Object) -> Option<&'o Dict> {
        match self.resolve(object) {
            Object::Dict(dict) | Object::Stream(dict, _) => Some(dict),
            _ => None,
        }
    }

    fn number(&self, object: Option<&Object>) -> Option<f64> {
        match self.resolve(object?) {
            Object::Number(number) => Some(*number),
            _ => None,
        }
    }

    fn whole(&self, object: Option<&Object>) -> Option<u32> {
        whole(self.number(object)?)
    }

    fn decode(&self, dict: &Dict, range: Range<usize>) -> Option<Vec<u8>> {
        let filters = match self.get(dict, b"Filter") {
            None | Some(Object::Other) => Vec::new(),
            Some(Object::Name(name)) => vec![name],
            Some(Object::Array(names)) => names
                .iter()
                .map(|name| match self.resolve(name) {
                    Object::Name(name) => Some(name),
                    _ => None,
                })
                .collect::<Option<_>>()?,
            Some(_) => return None,
        };
        let predicted = |parameters: &Object| {
            self.dict(parameters)
                .and_then(|parameters| self.number(parameters.get(b"Predictor")))
                .is_some_and(|predictor| predictor > 1.0)
        };
        match self.get(dict, b"DecodeParms") {
            Some(Object::Array(items)) if items.iter().any(predicted) => return None,
            Some(parameters) if predicted(parameters) => return None,
            _ => {}
        }
        let end = self
            .whole(dict.get(b"Length"))
            .and_then(|length| range.start.checked_add(length as usize))
            .filter(|end| ends_stream(self.data, *end))
            .unwrap_or(range.end);
        let mut data = self.data.get(range.start..end)?.to_vec();
        for filter in filters {
            if filter != b"FlateDecode" {
                return None;
            }
            let limit = MAX_INFLATED - self.inflated.get();
            data = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&data, limit).ok()?;
            self.inflated.set(self.inflated.get() + data.len());
        }
        Some(data)
    }

    /// The decoded data of the stream an object is or refers to.
    fn stream(&self, object: &Object) -> Option<Rc<[u8]>> {
        let id = match object {
            Object::Ref(id) => Some(*id),
            _ => None,
        };
        if let Some(data) = id.and_then(|id| self.streams.borrow().get(&id).cloned()) {
            return Some(data);
        }
        let Object::Stream(dict, range) = self.resolve(object) else {
            return None;
        };
        let data: Rc<[u8]> = self.decode(dict, range.clone())?.into();
        if let Some(id) = id {
            self.streams.borrow_mut().insert(id, data.clone());
        }
        Some(data)
    }

    /// The pages in order, each with the resources it has or inherits.
    fn pages(&self) -> Option<Vec<(&Dict, Option<&Dict>)>> {
        let catalog = self.dict(self.objects.get(&self.root)?)?;
        let mut pages = Vec::new();
        self.page_tree(catalog.get(b"Pages")?, None, 0, &mut HashSet::new(), &mut pages)?;
        Some(pages)
    }

    fn page_tree<'o>(
        &'o self,
        node: &'o Object,
        resources: Option<&'o Dict>,
        depth: usize,
        seen: &mut HashSet<u32>,
        pages: &mut Vec<(&'o Dict, Option<&'o Dict>)>,
    ) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        // A node listed twice is drawn once, so a looping tree ends.
        if let Object::Ref(id) = node
            && !seen.insert(*id)
        {
            return Some(());
        }
        let dict = self.dict(node)?;
        let resources = dict.get(b"Resources").and_then(|object| self.dict(object)).or(resources);
        match self.get(dict, b"Kids") {
            Some(Object::Array(kids)) => {
                for kid in kids {
                    self.page_tree(kid, resources, depth + 1, seen, pages)?;
                }
            }
            _ => pages.push((dict, resources)),
        }
        Some(())
    }

    /// A page's content streams, joined.
    fn contents(&self, page: &Dict) -> Option<Vec<u8>> {
        let Some(object) = page.get(b"Contents") else {
            return Some(Vec::new());
        };
        match self.resolve(object) {
            Object::Other => Some(Vec::new()),
            Object::Stream(..) => Some(self.stream(object)?.to_vec()),
            Object::Array(parts) => {
                let mut out = Vec::new();
                for part in parts {
                    out.extend_from_slice(&self.stream(part)?);
                    out.push(b'\n');
                }
                Some(out)
            }
            _ => None,
        }
    }
}

/// `[a b c d e f]`, mapping (x, y) to (ax + cy + e, bx + dy + f).
type Matrix = [f64; 6];

const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `first` then `second`.
fn multiply(first: &Matrix, second: &Matrix) -> Matrix {
    let [a, b, c, d, e, f] = *first;
    let [g, h, i, j, k, l] = *second;
    [
        a * g + b * i,
        a * h + b * j,
        c * g + d * i,
        c * h + d * j,
        e * g + f * i + k,
        e * h + f * j + l,
    ]
}

fn translation(x: f64, y: f64) -> Matrix {
    [1.0, 0.0, 0.0, 1.0, x, y]
}

fn matrix(operands: &[Object]) -> Option<Matrix> {
    let mut out = [0.0; 6];
    if operands.len() != 6 {
        return None;
    }
    for (slot, operand) in out.iter_mut().zip(operands) {
        let Object::Number(number) = operand else {
            return None;
        };
        *slot = *number;
    }
    Some(out)
}

fn operand(operands: &[Object], index: usize) -> Option<f64> {
    match operands.get(index) {
        Some(Object::Number(number)) => Some(*number),
        _ => None,
    }
}

#[derive(Clone)]
struct GraphicsState {
    ctm: Matrix,
    font: Option<Rc<Font>>,
    size: f64,
    char_spacing: f64,
    word_spacing: f64,
    scale: f64,
    leading: f64,
    rise: f64,
}

/// Glyphs on one baseline, in device space.
struct Line {
    /// The baseline's direction, a unit vector.
    direction: (f64, f64),
    /// The baseline's position across that direction; a lower line has less.
    across: f64,
    size: f64,
    /// Where the text starts and ends along the baseline.
    start: f64,
    end: f64,
    /// Where the first word ends, once a space follows it.
    first_word: Option<f64>,
    /// Whether a space glyph was drawn after the last text.
    space: bool,
    text: String,
}

/// The lines of the page being read.
struct Page<'d, 'a> {
    document: &'d Document<'a>,
    fonts: HashMap<u32, Rc<Font>>,
    content: usize,
    line: Option<Line>,
    lines: Vec<Line>,
}

impl<'d> Page<'d, '_> {
    fn run(&mut self, content: &[u8], resources: Option<&'d Dict>, ctm: Matrix, depth: usize) -> Option<()> {
        self.content += content.len();
        if depth > MAX_DEPTH || self.content > MAX_CONTENT {
            return None;
        }
        let mut lexer = Lexer::new(content, 0, false);
        let mut operands = Vec::new();
        let mut state = GraphicsState {
            ctm,
            font: None,
            size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            scale: 1.0,
            leading: 0.0,
            rise: 0.0,
        };
        let mut saved = Vec::new();
        let (mut tm, mut tlm) = (IDENTITY, IDENTITY);
        while !lexer.at_end() {
            let operator = match lexer.token()? {
                Token::Keyword(operator) => operator,
                token => {
                    if operands.len() == MAX_OPERANDS {
                        return None;
                    }
                    operands.push(lexer.object_from(token, 0)?);
                    continue;
                }
            };
            match operator {
                b"q" => {
                    if saved.len() == MAX_DEPTH {
                        return None;
                    }
                    saved.push(state.clone());
                }
                b"Q" => {
                    if let Some(previous) = saved.pop() {
                        state = previous;
                    }
                }
                b"cm" => {
                    if let Some(matrix) = matrix(&operands) {
                        state.ctm = multiply(&matrix, &state.ctm);
                    }
                }
                b"BT" => (tm, tlm) = (IDENTITY, IDENTITY),
                b"Tf" => {
                    if let (Some(Object::Name(name)), Some(size)) = (operands.first(), operand(&operands, 1)) {
                        state.font = self.font(resources, name);
                        state.size = size;
                    }
                }
                b"Tc" => state.char_spacing = operand(&operands, 0).unwrap_or(state.char_spacing),
                b"Tw" => state.word_spacing = operand(&operands, 0).unwrap_or(state.word_spacing),
                b"Tz" => state.scale = operand(&operands, 0).map_or(state.scale, |scale| scale / 100.0),
                b"TL" => state.leading = operand(&operands, 0).unwrap_or(state.leading),
                b"Ts" => state.rise = operand(&operands, 0).unwrap_or(state.rise),
                b"Td" | b"TD" => {
                    if let (Some(x), Some(y)) = (operand(&operands, 0), operand(&operands, 1)) {
                        if operator == b"TD" {
                            state.leading = -y;
                        }
                        tlm = multiply(&translation(x, y), &tlm);
                        tm = tlm;
                    }
                }
                b"Tm" => {
                    if let Some(matrix) = matrix(&operands) {
                        (tm, tlm) = (matrix, matrix);
                    }
                }
                b"T*" => {
                    tlm = multiply(&translation(0.0, -state.leading), &tlm);
                    tm = tlm;
                }
                b"Tj" | b"'" | b"\"" => {
                    if operator == b"\"" {
                        state.word_spacing = operand(&operands, 0).unwrap_or(state.word_spacing);
                        state.char_spacing = operand(&operands, 1).unwrap_or(state.char_spacing);
                    }
                    if operator != b"Tj" {
                        tlm = multiply(&translation(0.0, -state.leading), &tlm);
                        tm = tlm;
                    }
                    if let Some(Object::String(bytes)) = operands.last() {
                        self.show(&state, &mut tm, bytes);
                    }
                }
                b"TJ" => {
                    if let Some(Object::Array(items)) = operands.first() {
                        for item in items {
                            match item {
                                Object::String(bytes) => self.show(&state, &mut tm, bytes),
                                Object::Number(adjustment) => {
                                    let x = -adjustment / 1000.0 * state.size * state.scale;
                                    tm = multiply(&translation(x, 0.0), &tm);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                b"Do" => {
                    if let Some(Object::Name(name)) = operands.first() {
                        self.form(resources, name, &state.ctm, depth)?;
                    }
                }
                b"BI" => skip_inline_image(&mut lexer)?,
                _ => {}
            }
            operands.clear();
        }
        Some(())
    }

    /// Draws the form XObject `name`; other XObjects have no text.
    fn form(&mut self, resources: Option<&'d Dict>, name: &[u8], ctm: &Matrix, depth: usize) -> Option<()> {
        let document = self.document;
        let Some(object) = resources
            .and_then(|resources| resources.get(b"XObject"))
            .and_then(|xobjects| document.dict(xobjects))
            .and_then(|xobjects| xobjects.get(name))
        else {
            return Some(());
        };
        let Object::Stream(dict, _) = document.resolve(object) else {
            return Some(());
        };
        if !matches!(document.get(dict, b"Subtype"), Some(Object::Name(subtype)) if subtype == b"Form") {
            return Some(());
        }
        let content = document.stream(object)?;
        let matrix = match document.get(dict, b"Matrix") {
            Some(Object::Array(items)) => matrix(items).unwrap_or(IDENTITY),
            _ => IDENTITY,
        };
        let resources = dict.get(b"Resources").and_then(|object| document.dict(object)).or(resources);
        self.run(&content, resources, multiply(&matrix, ctm), depth + 1)
    }

    fn font(&mut self, resources: Option<&'d Dict>, name: &[u8]) -> Option<Rc<Font>> {
        let document = self.document;
        let object = resources
            .and_then(|resources| resources.get(b"Font"))
            .and_then(|fonts| document.dict(fonts))?
            .get(name)?;
        let id = match object {
            Object::Ref(id) => Some(*id),
            _ => None,
        };
        if let Some(font) = id.and_then(|id| self.fonts.get(&id)) {
            return Some(font.clone());
        }
        let font = Rc::new(Font::read(document, document.dict(object)?));
        if let Some(id) = id {
            self.fonts.insert(id, font.clone());
        }
        Some(font)
    }

    fn show(&mut self, state: &GraphicsState, tm: &mut Matrix, bytes: &[u8]) {
        let Some(font) = &state.font else {
            return;
        };
        let width = if font.two_byte { 2 } else { 1 };
        for code in bytes.chunks_exact(width) {
            let code = code.iter().fold(0, |code, byte| code << 8 | u32::from(*byte));
            let render = multiply(tm, &state.ctm);
            let origin = (state.rise * render[2] + render[4], state.rise * render[3] + render[5]);
            let length = render[0].hypot(render[1]);
            let direction = if length > 0.0 { (render[0] / length, render[1] / length) } else { (1.0, 0.0) };
            let size = (state.size * render[2].hypot(render[3])).abs();
            let mut advance = font.width(code) * state.size + state.char_spacing;
            if width == 1 && code == 32 {
                advance += state.word_spacing;
            }
            *tm = multiply(&translation(advance * state.scale, 0.0), tm);
            let render = multiply(tm, &state.ctm);
            let end = (state.rise * render[2] + render[4], state.rise * render[3] + render[5]);
            // Positions along the baseline and across it, so that rotated
            // text reads as lines too.
            let across = (-direction.1, direction.0);
            self.glyph(
                &font.text(code),
                direction,
                dot(origin, across),
                dot(origin, direction),
                dot(end, direction),
                size,
            );
        }
    }

    fn glyph(&mut self, text: &str, direction: (f64, f64), across: f64, start: f64, end: f64, size: f64) {
        let same_line = self.line.as_ref().is_some_and(|line| {
            let size = size.max(line.size);
            dot(direction, line.direction) > 0.99
                && (across - line.across).abs() <= 0.5 * size
                && start >= line.end - 0.5 * size
        });
        if !same_line {
            self.end_line();
        }
        let text: Cow<'_, str> = if text.chars().any(char::is_control) {
            text.chars().filter(|c| !c.is_control()).collect::<String>().into()
        } else {
            text.into()
        };
        if text.chars().all(char::is_whitespace) {
            if let Some(line) = &mut self.line {
                line.space = true;
            }
            return;
        }
        let Some(line) = &mut self.line else {
            self.line = Some(Line {
                direction,
                across,
                size,
                start,
                end,
                first_word: None,
                space: false,
                text: text.into_owned(),
            });
            return;
        };
        if line.space || start - line.end > WORD_GAP * size.max(line.size) {
            line.first_word.get_or_insert(line.end);
            line.text.push(' ');
        }
        line.space = false;
        line.text.push_str(&text);
        line.end = end;
        line.size = line.size.max(size);
    }

    fn end_line(&mut self) {
        self.lines.extend(self.line.take());
    }
}

/// Skips an inline image after `BI`: its dictionary, `ID`, its data and `EI`.
fn skip_inline_image(lexer: &mut Lexer<'_>) -> Option<()> {
    loop {
        match lexer.token()? {
            Token::Keyword(b"ID") => break,
            token => {
                lexer.object_from(token, 0)?;
            }
        }
    }
    // The data ends at an `EI` between white space.
    let mut at = lexer.pos;
    loop {
        at = find(lexer.data, b"EI", at)?;
        let before = at.checked_sub(1).map(|before| lexer.data[before]);
        let after = lexer.data.get(at + 2).copied();
        if before.is_some_and(is_space) && after.is_none_or(|byte| !is_regular(byte)) {
            lexer.pos = at + 2;
            return Some(());
        }
        at += 2;
    }
}

fn dot(first: (f64, f64), second: (f64, f64)) -> f64 {
    first.0 * second.0 + first.1 * second.1
}

/// A font size or a distance in half points, as a key.
fn half_points(value: f64) -> i64 {
    (value * 2.0).round() as i64
}

/// The usual line spacing of each font size, by size in half points: the
/// commonest drop from a line to the next in its block, over the whole
/// document. A drop well beyond it is a gap between paragraphs, even on a
/// page whose own lines never wrap.
fn line_spacing(pages: &[Vec<Line>]) -> BTreeMap<i64, f64> {
    let mut counts: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for lines in pages {
        for block in blocks(lines, &BTreeMap::new()) {
            for pair in block.windows(2) {
                let size = half_points(pair[0].size.max(pair[1].size));
                *counts.entry((size, half_points(pair[0].across - pair[1].across))).or_default() += 1;
            }
        }
    }
    let mut usual: BTreeMap<i64, (usize, f64)> = BTreeMap::new();
    for ((size, drop), count) in counts {
        let best = usual.entry(size).or_insert((0, 0.0));
        if count > best.0 {
            *best = (count, drop as f64 / 2.0);
        }
    }
    usual.into_iter().map(|(size, (_, drop))| (size, drop)).collect()
}

/// Groups lines into blocks: lines one below another in about the same size,
/// each overlapping the last horizontally, no further below it than 2.5 font
/// sizes or 1.3 times the usual spacing of their size.
fn blocks<'l>(lines: &'l [Line], spacing: &BTreeMap<i64, f64>) -> Vec<Vec<&'l Line>> {
    let mut blocks: Vec<Vec<&Line>> = Vec::new();
    for line in lines {
        let open = blocks.iter_mut().rev().take(OPEN_BLOCKS).find(|block| {
            let last = block.last().expect("a block has a line");
            continues(last, line, spacing)
        });
        match open {
            Some(block) => block.push(line),
            None => blocks.push(vec![line]),
        }
    }
    blocks
}

/// Writes a page's lines as paragraphs, one indented line each. Within a
/// block, a line ends its paragraph when the next line's first word would
/// have fit after it before the block's right edge: a wrapped line has no
/// room left.
fn paragraphs(lines: &[Line], spacing: &BTreeMap<i64, f64>, out: &mut String) {
    for block in blocks(lines, spacing) {
        let edge = block.iter().map(|line| line.end).fold(f64::MIN, f64::max);
        let mut paragraph = block[0].text.clone();
        for pair in block.windows(2) {
            let (previous, next) = (pair[0], pair[1]);
            let word = next.first_word.unwrap_or(next.end) - next.start;
            if previous.end + SPACE * previous.size + word <= edge {
                push_paragraph(out, &paragraph);
                paragraph.clone_from(&next.text);
            } else if let Some(stem) = paragraph.strip_suffix('\u{ad}').map(str::len) {
                // A soft hyphen only marks where the word was broken.
                paragraph.truncate(stem);
                paragraph.push_str(&next.text);
            } else if paragraph.ends_with('-') && paragraph.chars().rev().nth(1).is_some_and(char::is_alphabetic) {
                paragraph.push_str(&next.text);
            } else {
                paragraph.push(' ');
                paragraph.push_str(&next.text);
            }
        }
        push_paragraph(out, &paragraph);
    }
}

fn continues(previous: &Line, next: &Line, spacing: &BTreeMap<i64, f64>) -> bool {
    let size = previous.size.max(next.size);
    let drop = previous.across - next.across;
    let usual = spacing.get(&half_points(size)).copied().unwrap_or(f64::INFINITY);
    dot(previous.direction, next.direction) > 0.99
        && drop > 0.5 * size
        && drop <= 2.5 * size
        && drop <= 1.3 * usual
        && (previous.size - next.size).abs() <= 0.2 * size
        && next.start < previous.end
        && previous.start < next.end
}

fn push_paragraph(out: &mut String, paragraph: &str) {
    out.push_str("  ");
    out.push_str(paragraph);
    out.push('\n');
}

struct Font {
    /// Type 0 fonts read two bytes per code; others one.
    two_byte: bool,
    unicode: ToUnicode,
    /// The text of each one-byte code by the font's encoding.
    encoding: Vec<String>,
    widths: Widths,
}

impl Font {
    fn read(document: &Document<'_>, dict: &Dict) -> Font {
        let unicode = dict
            .get(b"ToUnicode")
            .and_then(|object| document.stream(object))
            .map(|data| ToUnicode::read(&data))
            .unwrap_or_default();
        if dict.is(b"Subtype", b"Type0") {
            let descendant = match document.get(dict, b"DescendantFonts") {
                Some(Object::Array(fonts)) => fonts.first().and_then(|font| document.dict(font)),
                _ => None,
            };
            return Font {
                two_byte: true,
                unicode,
                encoding: Vec::new(),
                widths: descendant.map_or_else(Widths::default, |font| Widths::cid(document, font)),
            };
        }
        let scale = match document.get(dict, b"FontMatrix") {
            Some(Object::Array(matrix)) if dict.is(b"Subtype", b"Type3") => {
                document.number(matrix.first()).unwrap_or(0.001)
            }
            _ => 0.001,
        };
        Font {
            two_byte: false,
            unicode,
            encoding: simple_encoding(document, dict),
            widths: Widths::simple(document, dict, scale),
        }
    }

    fn text(&self, code: u32) -> Cow<'_, str> {
        if let Some(text) = self.unicode.get(code) {
            return text;
        }
        match self.encoding.get(code as usize) {
            Some(text) => Cow::Borrowed(text),
            None => Cow::Borrowed("\u{fffd}"),
        }
    }

    /// The glyph's advance in text space units, per unit of font size.
    fn width(&self, code: u32) -> f64 {
        self.widths.get(code)
    }
}

enum Destination {
    /// The text of the first code; each next code adds one to its last unit.
    Start(Vec<u16>),
    List(Vec<String>),
}

/// A ToUnicode CMap.
#[derive(Default)]
struct ToUnicode {
    chars: HashMap<u32, String>,
    /// Ranges by their first code.
    ranges: BTreeMap<u32, (u32, Destination)>,
}

impl ToUnicode {
    fn read(data: &[u8]) -> ToUnicode {
        let mut map = ToUnicode::default();
        let mut lexer = Lexer::new(data, 0, false);
        while let Some(token) = lexer.token() {
            match token {
                Token::Keyword(b"beginbfchar") => loop {
                    let Some(Token::Object(Object::String(source))) = lexer.token() else {
                        break;
                    };
                    let Some(Token::Object(Object::String(text))) = lexer.token() else {
                        break;
                    };
                    if let Some(code) = code(&source) {
                        map.chars.insert(code, utf16(&units(&text)));
                    }
                },
                Token::Keyword(b"beginbfrange") => loop {
                    let Some(Token::Object(Object::String(low))) = lexer.token() else {
                        break;
                    };
                    let Some(Token::Object(Object::String(high))) = lexer.token() else {
                        break;
                    };
                    let destination = match lexer.object(0) {
                        Some(Object::String(text)) => Destination::Start(units(&text)),
                        Some(Object::Array(items)) => Destination::List(
                            items
                                .iter()
                                .map(|item| match item {
                                    Object::String(text) => utf16(&units(text)),
                                    _ => "\u{fffd}".to_string(),
                                })
                                .collect(),
                        ),
                        _ => break,
                    };
                    if let (Some(low), Some(high)) = (code(&low), code(&high))
                        && low <= high
                    {
                        map.ranges.insert(low, (high, destination));
                    }
                },
                _ => {}
            }
        }
        map
    }

    fn get(&self, code: u32) -> Option<Cow<'_, str>> {
        if let Some(text) = self.chars.get(&code) {
            return Some(Cow::Borrowed(text));
        }
        let (low, (high, destination)) = self.ranges.range(..=code).next_back()?;
        if code > *high {
            return None;
        }
        let offset = code - low;
        match destination {
            Destination::List(texts) => texts.get(offset as usize).map(|text| Cow::Borrowed(text.as_str())),
            Destination::Start(units) => {
                let mut units = units.clone();
                if let Some(last) = units.last_mut() {
                    *last = last.wrapping_add(offset as u16);
                }
                Some(Cow::Owned(utf16(&units)))
            }
        }
    }
}

fn code(bytes: &[u8]) -> Option<u32> {
    (!bytes.is_empty() && bytes.len() <= 4).then(|| bytes.iter().fold(0, |code, byte| code << 8 | u32::from(*byte)))
}

/// UTF-16BE code units; a lone final byte is a unit of its own.
fn units(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks(2)
        .map(|pair| pair.iter().fold(0, |unit, byte| unit << 8 | u16::from(*byte)))
        .collect()
}

fn utf16(units: &[u16]) -> String {
    char::decode_utf16(units.iter().copied())
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

/// Glyph widths in text space units per unit of font size, as runs by their
/// first code.
struct Widths {
    default: f64,
    runs: BTreeMap<u32, (u32, Vec<f64>)>,
}

impl Default for Widths {
    fn default() -> Self {
        Widths { default: 1.0, runs: BTreeMap::new() }
    }
}

impl Widths {
    /// `/FirstChar` and `/Widths`. A font without them is one of the standard
    /// fonts, whose widths are not here: every glyph gets half its size.
    fn simple(document: &Document<'_>, font: &Dict, scale: f64) -> Widths {
        let first = document.whole(font.get(b"FirstChar"));
        let Some(Object::Array(widths)) = document.get(font, b"Widths") else {
            return Widths { default: 500.0 * scale, runs: BTreeMap::new() };
        };
        let missing = document
            .get(font, b"FontDescriptor")
            .and_then(|descriptor| document.dict(descriptor))
            .and_then(|descriptor| document.number(descriptor.get(b"MissingWidth")))
            .unwrap_or(0.0);
        let widths: Vec<f64> = widths
            .iter()
            .map(|width| document.number(Some(width)).unwrap_or(missing) * scale)
            .collect();
        let mut runs = BTreeMap::new();
        if let Some(first) = first
            && !widths.is_empty()
        {
            runs.insert(first, (first.saturating_add(widths.len() as u32 - 1), widths));
        }
        Widths { default: missing * scale, runs }
    }

    /// A CIDFont's `/DW` and `/W`: `c [w1 w2 ...]` gives codes from c their
    /// widths in turn, `c1 c2 w` gives codes c1 to c2 the width w.
    fn cid(document: &Document<'_>, font: &Dict) -> Widths {
        let default = document.number(font.get(b"DW")).unwrap_or(1000.0) / 1000.0;
        let mut runs = BTreeMap::new();
        if let Some(Object::Array(items)) = document.get(font, b"W") {
            let mut items = items.iter().map(|item| document.resolve(item));
            while let Some(Object::Number(first)) = items.next() {
                let Some(first) = whole(*first) else {
                    break;
                };
                match items.next() {
                    Some(Object::Array(widths)) if !widths.is_empty() => {
                        let widths: Vec<f64> = widths
                            .iter()
                            .map(|width| document.number(Some(width)).unwrap_or(0.0) / 1000.0)
                            .collect();
                        runs.insert(first, (first.saturating_add(widths.len() as u32 - 1), widths));
                    }
                    Some(Object::Number(last)) => {
                        let (Some(last), Some(width)) = (whole(*last), document.number(items.next())) else {
                            break;
                        };
                        if first <= last {
                            runs.insert(first, (last, vec![width / 1000.0]));
                        }
                    }
                    _ => break,
                }
            }
        }
        Widths { default, runs }
    }

    fn get(&self, code: u32) -> f64 {
        match self.runs.range(..=code).next_back() {
            Some((first, (last, widths))) if code <= *last => {
                let index = (code - first) as usize;
                widths.get(index).or(widths.last()).copied().unwrap_or(self.default)
            }
            _ => self.default,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum BaseEncoding {
    /// Only the ASCII half of StandardEncoding: the other has few letters.
    Standard,
    WinAnsi,
    MacRoman,
    /// A symbol font's own codes, which are not Latin text.
    Symbol,
}

/// The text of each code of a one-byte font: its base encoding changed by
/// `/Differences`.
fn simple_encoding(document: &Document<'_>, font: &Dict) -> Vec<String> {
    let (base, differences) = match document.get(font, b"Encoding") {
        Some(Object::Name(name)) => (Some(name.as_slice()), None),
        Some(Object::Dict(encoding)) => {
            let base = match document.get(encoding, b"BaseEncoding") {
                Some(Object::Name(name)) => Some(name.as_slice()),
                _ => None,
            };
            (base, document.get(encoding, b"Differences"))
        }
        _ => (None, None),
    };
    let symbol = match document.get(font, b"BaseFont") {
        Some(Object::Name(name)) => {
            let name = String::from_utf8_lossy(name);
            name.contains("Symbol") || name.contains("Dingbats")
        }
        _ => false,
    };
    let base = match base {
        _ if symbol => BaseEncoding::Symbol,
        Some(b"WinAnsiEncoding") => BaseEncoding::WinAnsi,
        Some(b"MacRomanEncoding") => BaseEncoding::MacRoman,
        _ => BaseEncoding::Standard,
    };
    let mut table: Vec<String> = (0..=255)
        .map(|code| base_char(base, code).map_or_else(|| "\u{fffd}".to_string(), String::from))
        .collect();
    if let Some(Object::Array(items)) = differences {
        let mut code = 0;
        for item in items {
            match document.resolve(item) {
                Object::Number(number) => code = whole(*number).map_or(usize::MAX, |code| code as usize),
                Object::Name(name) => {
                    if let Some(slot) = table.get_mut(code) {
                        *slot = glyph_text(name);
                    }
                    code = code.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    table
}

/// WinAnsiEncoding's 0x80 to 0x9f: each character with its glyph name.
const WIN_ANSI_80: [(char, &str); 32] = [
    ('€', "Euro"),
    ('\0', ""),
    ('‚', "quotesinglbase"),
    ('ƒ', "florin"),
    ('„', "quotedblbase"),
    ('…', "ellipsis"),
    ('†', "dagger"),
    ('‡', "daggerdbl"),
    ('ˆ', "circumflex"),
    ('‰', "perthousand"),
    ('Š', "Scaron"),
    ('‹', "guilsinglleft"),
    ('Œ', "OE"),
    ('\0', ""),
    ('Ž', "Zcaron"),
    ('\0', ""),
    ('\0', ""),
    ('\u{2018}', "quoteleft"),
    ('\u{2019}', "quoteright"),
    ('“', "quotedblleft"),
    ('”', "quotedblright"),
    ('•', "bullet"),
    ('–', "endash"),
    ('—', "emdash"),
    ('˜', "tilde"),
    ('™', "trademark"),
    ('š', "scaron"),
    ('›', "guilsinglright"),
    ('œ', "oe"),
    ('\0', ""),
    ('ž', "zcaron"),
    ('Ÿ', "Ydieresis"),
];

/// Glyph names of U+00A1 to U+00FF.
const LATIN_1_NAMES: [&str; 95] = [
    "exclamdown", "cent", "sterling", "currency", "yen", "brokenbar", "section", "dieresis", "copyright",
    "ordfeminine", "guillemotleft", "logicalnot", "sfthyphen", "registered", "macron", "degree", "plusminus",
    "twosuperior", "threesuperior", "acute", "mu", "paragraph", "periodcentered", "cedilla", "onesuperior",
    "ordmasculine", "guillemotright", "onequarter", "onehalf", "threequarters", "questiondown", "Agrave",
    "Aacute", "Acircumflex", "Atilde", "Adieresis", "Aring", "AE", "Ccedilla", "Egrave", "Eacute",
    "Ecircumflex", "Edieresis", "Igrave", "Iacute", "Icircumflex", "Idieresis", "Eth", "Ntilde", "Ograve",
    "Oacute", "Ocircumflex", "Otilde", "Odieresis", "multiply", "Oslash", "Ugrave", "Uacute", "Ucircumflex",
    "Udieresis", "Yacute", "Thorn", "germandbls", "agrave", "aacute", "acircumflex", "atilde", "adieresis",
    "aring", "ae", "ccedilla", "egrave", "eacute", "ecircumflex", "edieresis", "igrave", "iacute",
    "icircumflex", "idieresis", "eth", "ntilde", "ograve", "oacute", "ocircumflex", "otilde", "odieresis",
    "divide", "oslash", "ugrave", "uacute", "ucircumflex", "udieresis", "yacute", "thorn", "ydieresis",
];

/// MacRomanEncoding's 0x80 to 0xff, with U+FFFD for 0xf0.
const MAC_ROMAN_80: &str = "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»… ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄¤‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ�ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";

fn base_char(base: BaseEncoding, code: u8) -> Option<char> {
    match (base, code) {
        (BaseEncoding::Symbol, _) => None,
        (BaseEncoding::Standard, b'\'') => Some('\u{2019}'),
        (BaseEncoding::Standard, b'`') => Some('\u{2018}'),
        (_, 0x20..=0x7e) => Some(char::from(code)),
        (BaseEncoding::Standard, _) => None,
        (BaseEncoding::MacRoman, 0x80..=0xff) => MAC_ROMAN_80.chars().nth(usize::from(code - 0x80)),
        (BaseEncoding::MacRoman, _) => None,
        (BaseEncoding::WinAnsi, 0xa1..=0xac | 0xae..=0xff) => Some(char::from(code)),
        (BaseEncoding::WinAnsi, 0xa0) => Some(' '),
        (BaseEncoding::WinAnsi, 0xad) => Some('-'),
        (BaseEncoding::WinAnsi, 0x80..=0x9f) => Some(WIN_ANSI_80[usize::from(code - 0x80)].0).filter(|c| *c != '\0'),
        (BaseEncoding::WinAnsi, _) => None,
    }
}

/// The text of a glyph name: an Adobe Glyph List name for Latin text,
/// `uniXXXX` or `uXXXX`. A suffix after a period is a variant of the same
/// glyph.
fn glyph_text(name: &[u8]) -> String {
    let name = std::str::from_utf8(name).unwrap_or("");
    let name = name.split('.').next().unwrap_or("");
    let ascii = match name {
        "space" => " ",
        "exclam" => "!",
        "quotedbl" => "\"",
        "numbersign" => "#",
        "dollar" => "$",
        "percent" => "%",
        "ampersand" => "&",
        "quotesingle" => "'",
        "parenleft" => "(",
        "parenright" => ")",
        "asterisk" => "*",
        "plus" => "+",
        "comma" => ",",
        "hyphen" => "-",
        "minus" => "\u{2212}",
        "period" => ".",
        "slash" => "/",
        "zero" => "0",
        "one" => "1",
        "two" => "2",
        "three" => "3",
        "four" => "4",
        "five" => "5",
        "six" => "6",
        "seven" => "7",
        "eight" => "8",
        "nine" => "9",
        "colon" => ":",
        "semicolon" => ";",
        "less" => "<",
        "equal" => "=",
        "greater" => ">",
        "question" => "?",
        "at" => "@",
        "bracketleft" => "[",
        "backslash" => "\\",
        "bracketright" => "]",
        "asciicircum" => "^",
        "underscore" => "_",
        "grave" => "`",
        "braceleft" => "{",
        "bar" => "|",
        "braceright" => "}",
        "asciitilde" => "~",
        "nbspace" => " ",
        "ff" | "fi" | "fl" | "ffi" | "ffl" => name,
        "dotlessi" => "ı",
        "Lslash" => "Ł",
        "lslash" => "ł",
        "fraction" => "⁄",
        _ => "",
    };
    if !ascii.is_empty() {
        return ascii.to_string();
    }
    if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
        return name.to_string();
    }
    if let Some(index) = LATIN_1_NAMES.iter().position(|latin| *latin == name) {
        return char::from_u32(0xa1 + index as u32).expect("Latin-1 is Unicode").to_string();
    }
    if let Some((c, _)) = WIN_ANSI_80.iter().find(|(_, glyph)| !glyph.is_empty() && *glyph == name) {
        return c.to_string();
    }
    if let Some(hex) = name.strip_prefix("uni")
        && !hex.is_empty()
        && hex.len() % 4 == 0
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let units: Option<Vec<u16>> = hex
            .as_bytes()
            .chunks(4)
            .map(|digits| u16::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok())
            .collect();
        if let Some(units) = units {
            return utf16(&units);
        }
    }
    if let Some(hex) = name.strip_prefix('u')
        && (4..=6).contains(&hex.len())
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
        && let Some(c) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
    {
        return c.to_string();
    }
    "\u{fffd}".to_string()
}


#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PDF of numbered objects and a trailer, without a cross-reference
    /// table: the reader does not use one.
    pub(crate) fn pdf(objects: &[(u32, Vec<u8>)], trailer: &str) -> Vec<u8> {
        let mut out = b"%PDF-1.7\n".to_vec();
        for (id, object) in objects {
            out.extend(format!("{id} 0 obj\n").bytes());
            out.extend(object);
            out.extend(b"\nendobj\n");
        }
        out.extend(format!("trailer\n{trailer}\n%%EOF\n").bytes());
        out
    }

    pub(crate) fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        out.extend(data);
        out.extend(b"\nendstream");
        out
    }

    /// A PDF with a page for each content stream, drawn in a WinAnsi
    /// Helvetica without widths: each glyph is half the font size wide.
    pub(crate) fn pages(contents: &[&str]) -> Vec<u8> {
        let kids: Vec<String> = (0..contents.len()).map(|page| format!("{} 0 R", 4 + 2 * page)).collect();
        let mut objects = vec![
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (
                2,
                format!(
                    "<< /Type /Pages /Kids [{}] /Count {} /Resources << /Font << /F1 3 0 R >> >> >>",
                    kids.join(" "),
                    contents.len()
                )
                .into_bytes(),
            ),
            (
                3,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec(),
            ),
        ];
        for (page, content) in contents.iter().enumerate() {
            let id = 4 + 2 * page as u32;
            objects.push((id, format!("<< /Type /Page /Parent 2 0 R /Contents {} 0 R >>", id + 1).into_bytes()));
            objects.push((id + 1, stream("", content.as_bytes())));
        }
        pdf(&objects, "<< /Root 1 0 R >>")
    }

    #[test]
    fn wrapped_lines_join_into_paragraphs() {
        // Line numbers are drawn after each line, left of the text.
        let first = "BT /F1 10 Tf
            1 0 0 1 72 700 Tm (The quick brown fox) Tj 1 0 0 1 50 700 Tm (1) Tj
            1 0 0 1 72 688 Tm (jumps over the lazy) Tj 1 0 0 1 50 688 Tm (2) Tj
            1 0 0 1 72 676 Tm (dog.) Tj 1 0 0 1 50 676 Tm (3) Tj
            1 0 0 1 72 664 Tm (New paragraph) Tj 1 0 0 1 50 664 Tm (4) Tj ET";
        let second = "BT /F1 10 Tf 12 TL 72 700 Td (A long line that breaks to-) Tj T* (gether here) Tj
            1 0 0 1 72 500 Tm [(Hel) -20 (lo) -300 (world)] TJ
            0 1 -1 0 300 300 Tm (Rotated) Tj ET";
        assert_eq!(
            document_text(&pages(&[first, second])).unwrap(),
            "Page 1\n  The quick brown fox jumps over the lazy dog.\n  New paragraph\n  1 2 3 4\n\
             Page 2\n  A long line that breaks to-gether here\n  Hello world\n  Rotated\n"
        );
    }

    #[test]
    fn fonts_map_codes_to_text() {
        let cmap = b"begincmap 2 beginbfchar <0001> <0048> <0002> <00E9> endbfchar
            2 beginbfrange <0010> <0012> <0061> <0020> <0021> [<00660069> <0041>] endbfrange endcmap";
        let objects = [
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (
                3,
                b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font <<
                    /F1 5 0 R /F2 6 0 R /F3 7 0 R >> >> >>"
                    .to_vec(),
            ),
            (
                4,
                stream(
                    "",
                    b"BT /F1 10 Tf 72 700 Td <0001000200100011001200200021> Tj ET
                      BT /F2 10 Tf 72 600 Td (ABCD) Tj ET
                      BT /F3 10 Tf 72 500 Td (It\\325s) Tj ET",
                ),
            ),
            (
                5,
                b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /ToUnicode 8 0 R
                    /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /DW 500 >>] >>"
                    .to_vec(),
            ),
            (
                6,
                b"<< /Type /Font /Subtype /Type1 /BaseFont /Times-Roman
                    /Encoding << /Differences [65 /quoteright /uni00E9 /fi.alt /g42] >> >>"
                    .to_vec(),
            ),
            (7, b"<< /Type /Font /Subtype /TrueType /BaseFont /Times /Encoding /MacRomanEncoding >>".to_vec()),
            (8, stream("", cmap)),
        ];
        assert_eq!(
            document_text(&pdf(&objects, "<< /Root 1 0 R >>")).unwrap(),
            "Page 1\n  HéabcfiA\n  ’éfi\u{fffd}\n  It’s\n"
        );
    }

    #[test]
    fn later_definitions_and_object_streams_are_read_without_a_cross_reference_table() {
        let content = |text: &str| {
            let data = format!("BT /F1 10 Tf 72 700 Td ({text}) Tj ET /X1 Do");
            miniz_oxide::deflate::compress_to_vec_zlib(data.as_bytes(), 6)
        };
        let compressed = |id: u32, data: Vec<u8>| {
            let mut object = format!("<< /Filter /FlateDecode /Length {id} 0 R >>\nstream\n").into_bytes();
            object.extend(&data);
            object.extend(b"\nendstream");
            object
        };
        // The page and its font are in an object stream.
        let page = "<< /Type /Page /Parent 2 0 R /Contents 6 0 R
            /Resources << /Font << /F1 4 0 R >> /XObject << /X1 8 0 R >> >> >>";
        let font = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";
        let header = format!("3 0 4 {} ", page.len() + 1);
        let held = format!("{header}{page} {font}");
        let old = content("Old text");
        let new = content("New text");
        let mut file = pdf(
            &[
                (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
                (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
                (5, stream(&format!("/Type /ObjStm /N 2 /First {}", header.len()), held.as_bytes())),
                (6, compressed(7, old.clone())),
                (7, old.len().to_string().into_bytes()),
                (
                    8,
                    stream(
                        "/Type /XObject /Subtype /Form /BBox [0 0 600 800] /Matrix [1 0 0 1 0 -100]",
                        b"BT /F1 10 Tf 72 700 Td (In a form) Tj ET",
                    ),
                ),
            ],
            "<< /Root 1 0 R >>",
        );
        // An appended update replaces the content and its length.
        for (id, object) in [(6, compressed(9, new.clone())), (9, new.len().to_string().into_bytes())] {
            file.extend(format!("{id} 0 obj\n").bytes());
            file.extend(object);
            file.extend(b"\nendobj\n");
        }
        file.extend(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
        assert_eq!(document_text(&file).unwrap(), "Page 1\n  New text\n  In a form\n");
    }

    #[test]
    fn unreadable_documents_have_no_text() {
        let readable = pages(&["BT /F1 10 Tf 72 700 Td (Text) Tj ET"]);
        assert_eq!(document_text(&readable).unwrap(), "Page 1\n  Text\n");
        assert_eq!(document_text(b"not a PDF"), None);
        assert_eq!(document_text(&readable[..readable.len() / 2]), None);
        let mut encrypted = readable.clone();
        encrypted.extend(b"trailer\n<< /Root 1 0 R /Encrypt 9 0 R >>\n");
        assert_eq!(document_text(&encrypted), None);
        let lzw = pdf(
            &[
                (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
                (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
                (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec()),
                (4, stream("/Filter /LZWDecode", b"\x80\x0b\x60\x50\x22\x0c\x0c\x85\x01")),
            ],
            "<< /Root 1 0 R >>",
        );
        assert_eq!(document_text(&lzw), None);
    }
}
