//! Minimal hand-rolled Parquet writer: one row group, one PLAIN data page (v1)
//! per column, UNCOMPRESSED, no statistics. Supports required BYTE_ARRAY(STRING),
//! INT32(DATE), INT64 and optional BYTE_ARRAY(STRING).

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Str,
    OptStr,
    Date,
    I64,
}

pub struct ColBuf {
    pub name: &'static str,
    pub kind: Kind,
    /// PLAIN-encoded values
    pub data: Vec<u8>,
    /// definition levels (OptStr only), one byte per row (0/1)
    pub def: Vec<u8>,
}

impl ColBuf {
    pub fn new(name: &'static str, kind: Kind) -> Self {
        ColBuf { name, kind, data: Vec::new(), def: Vec::new() }
    }
    #[inline]
    pub fn push_str(&mut self, s: &[u8]) {
        self.data.extend_from_slice(&(s.len() as u32).to_le_bytes());
        self.data.extend_from_slice(s);
        if self.kind == Kind::OptStr {
            self.def.push(1);
        }
    }
    #[inline]
    pub fn push_null(&mut self) {
        debug_assert!(self.kind == Kind::OptStr);
        self.def.push(0);
    }
    #[inline]
    pub fn push_i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
    #[inline]
    pub fn push_i64(&mut self, v: i64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }
}

// ---- thrift compact protocol ----
struct W {
    b: Vec<u8>,
    last: Vec<i16>,
}
const T_I32: u8 = 5;
const T_I64: u8 = 6;
const T_BIN: u8 = 8;
const T_LIST: u8 = 9;
const T_STRUCT: u8 = 12;

impl W {
    fn new() -> Self {
        W { b: Vec::new(), last: vec![0] }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.b.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.b.push(v as u8);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn fh(&mut self, id: i16, t: u8) {
        let last = *self.last.last().unwrap();
        let d = id - last;
        if d > 0 && d <= 15 {
            self.b.push(((d as u8) << 4) | t);
        } else {
            self.b.push(t);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.fh(id, T_I32);
        self.zz(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.fh(id, T_I64);
        self.zz(v);
    }
    fn binf(&mut self, id: i16, s: &[u8]) {
        self.fh(id, T_BIN);
        self.varint(s.len() as u64);
        self.b.extend_from_slice(s);
    }
    fn begin(&mut self, id: i16) {
        self.fh(id, T_STRUCT);
        self.last.push(0);
    }
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.b.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, et: u8, n: usize) {
        self.fh(id, T_LIST);
        if n < 15 {
            self.b.push(((n as u8) << 4) | et);
        } else {
            self.b.push(0xF0 | et);
            self.varint(n as u64);
        }
    }
}

const PLAIN: i32 = 0;
const RLE: i32 = 3;

fn phys(k: Kind) -> i32 {
    match k {
        Kind::Str | Kind::OptStr => 6,
        Kind::Date => 1,
        Kind::I64 => 2,
    }
}

/// Serialize a whole parquet file to bytes. All columns must have `nrows` rows.
pub fn encode(cols: &[ColBuf], nrows: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(cols.iter().map(|c| c.data.len() + c.def.len() / 8 + 64).sum::<usize>() + 4096);
    out.extend_from_slice(b"PAR1");
    // (data_page_offset, total_size)
    let mut chunks: Vec<(i64, i64)> = vec![];
    if nrows > 0 {
        for c in cols {
            // page body
            let mut levels: Vec<u8> = vec![];
            if c.kind == Kind::OptStr {
                // RLE/bit-packed hybrid, bit width 1, one bit-packed run
                let groups = nrows.div_ceil(8);
                let mut hdr = W::new();
                hdr.varint(((groups as u64) << 1) | 1);
                let mut packed = vec![0u8; groups];
                for (i, &d) in c.def.iter().enumerate() {
                    packed[i >> 3] |= (d & 1) << (i & 7);
                }
                let len = hdr.b.len() + packed.len();
                levels.extend_from_slice(&(len as u32).to_le_bytes());
                levels.extend_from_slice(&hdr.b);
                levels.extend_from_slice(&packed);
            }
            let body_len = levels.len() + c.data.len();
            let mut ph = W::new();
            ph.i32f(1, 0); // DATA_PAGE
            ph.i32f(2, body_len as i32);
            ph.i32f(3, body_len as i32);
            ph.begin(5);
            ph.i32f(1, nrows as i32);
            ph.i32f(2, PLAIN);
            ph.i32f(3, RLE);
            ph.i32f(4, RLE);
            ph.end();
            ph.b.push(0);
            let off = out.len() as i64;
            out.extend_from_slice(&ph.b);
            out.extend_from_slice(&levels);
            out.extend_from_slice(&c.data);
            chunks.push((off, out.len() as i64 - off));
        }
    }
    // FileMetaData
    let mut m = W::new();
    m.i32f(1, 1);
    m.list(2, T_STRUCT, cols.len() + 1);
    m.begin_elem();
    m.binf(4, b"schema");
    m.i32f(5, cols.len() as i32);
    m.end();
    for c in cols {
        m.begin_elem();
        m.i32f(1, phys(c.kind));
        m.i32f(3, if c.kind == Kind::OptStr { 1 } else { 0 });
        m.binf(4, c.name.as_bytes());
        match c.kind {
            Kind::Str | Kind::OptStr => {
                m.i32f(6, 0); // UTF8
                m.begin(10);
                m.begin(1); // STRING
                m.end();
                m.end();
            }
            Kind::Date => {
                m.i32f(6, 6); // DATE
                m.begin(10);
                m.begin(6); // DATE
                m.end();
                m.end();
            }
            Kind::I64 => {}
        }
        m.end();
    }
    m.i64f(3, nrows as i64);
    if nrows > 0 {
        m.list(4, T_STRUCT, 1);
        m.begin_elem();
        m.list(1, T_STRUCT, cols.len());
        for (c, &(off, size)) in cols.iter().zip(chunks.iter()) {
            m.begin_elem();
            m.i64f(2, off);
            m.begin(3);
            m.i32f(1, phys(c.kind));
            if c.kind == Kind::OptStr {
                m.list(2, T_I32, 2);
                m.zz(PLAIN as i64);
                m.zz(RLE as i64);
            } else {
                m.list(2, T_I32, 1);
                m.zz(PLAIN as i64);
            }
            m.list(3, T_BIN, 1);
            m.varint(c.name.len() as u64);
            m.b.extend_from_slice(c.name.as_bytes());
            m.i32f(4, 0); // UNCOMPRESSED
            m.i64f(5, nrows as i64);
            m.i64f(6, size);
            m.i64f(7, size);
            m.i64f(9, off);
            m.end();
            m.end();
        }
        let total: i64 = chunks.iter().map(|x| x.1).sum();
        m.i64f(2, total);
        m.i64f(3, nrows as i64);
        m.end();
    } else {
        m.list(4, T_STRUCT, 0);
    }
    m.binf(6, b"h4-fast");
    m.b.push(0);
    out.extend_from_slice(&m.b);
    out.extend_from_slice(&(m.b.len() as u32).to_le_bytes());
    out.extend_from_slice(b"PAR1");
    out
}

pub fn user_cols() -> Vec<ColBuf> {
    vec![
        ColBuf::new("user_id", Kind::Str),
        ColBuf::new("day", Kind::Date),
        ColBuf::new("sessions", Kind::I64),
        ColBuf::new("events", Kind::I64),
        ColBuf::new("revenue_cents", Kind::I64),
        ColBuf::new("top_category", Kind::OptStr),
    ]
}

pub fn camp_cols() -> Vec<ColBuf> {
    vec![
        ColBuf::new("campaign_id", Kind::Str),
        ColBuf::new("channel", Kind::Str),
        ColBuf::new("day", Kind::Date),
        ColBuf::new("sessions", Kind::I64),
        ColBuf::new("purchasing_sessions", Kind::I64),
        ColBuf::new("revenue_cents", Kind::I64),
        ColBuf::new("assisted_sessions", Kind::I64),
    ]
}
