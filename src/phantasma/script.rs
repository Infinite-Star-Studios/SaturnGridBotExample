//! A minimal Phantasma VM script builder: contract calls and gas, exactly as
//! phantasma-sdk-ts emits them (byte-for-byte; see tests/vectors.rs).
//!
//! A contract call pushes its arguments in reverse order, then the method
//! name, then loads the contract name and switches context to it:
//!
//! ```text
//! LOAD r0 <arg n> ; PUSH r0 ... LOAD r0 <arg 1> ; PUSH r0
//! LOAD r0 "<method>" ; PUSH r0
//! LOAD r0 "<contract>" ; CTX r0 r1 ; SWITCH r1
//! ```
//!
//! Numbers go to contracts as decimal strings (the VM converts them), which
//! is what the Saturn website and Saturn's own bot kit send.

use super::keys::Address;

const OP_PUSH: u8 = 3;
const OP_RET: u8 = 11;
const OP_LOAD: u8 = 13;
const OP_CTX: u8 = 45;
const OP_SWITCH: u8 = 46;

const VM_BYTES: u8 = 2;
const VM_STRING: u8 = 4;

/// Phantasma's variable-length integer (the Bitcoin "CompactSize" layout).
pub fn write_varint(out: &mut Vec<u8>, n: u64) {
    if n < 0xfd {
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(0xfd);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= 0xffff_ffff {
        out.push(0xfe);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(0xff);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

/// A length-prefixed byte string.
pub fn write_var_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// One argument of a contract call.
#[derive(Clone, Copy, Debug)]
pub enum Arg<'a> {
    /// A string (also used for decimal numbers and for addresses in text form).
    Str(&'a str),
    /// An address as an object (its serialized bytes), as the Saturn website sends `from`.
    Addr(&'a Address),
}

#[derive(Default)]
pub struct ScriptBuilder {
    buf: Vec<u8>,
}

impl ScriptBuilder {
    pub fn new() -> Self {
        ScriptBuilder { buf: Vec::with_capacity(512) }
    }

    fn load(&mut self, reg: u8, vm_type: u8, data: &[u8]) {
        self.buf.push(OP_LOAD);
        self.buf.push(reg);
        self.buf.push(vm_type);
        write_var_bytes(&mut self.buf, data);
    }

    fn load_arg(&mut self, reg: u8, arg: Arg<'_>) {
        match arg {
            Arg::Str(s) => self.load(reg, VM_STRING, s.as_bytes()),
            Arg::Addr(a) => {
                // An Address serializes as a length-prefixed 34-byte array.
                let mut ser = Vec::with_capacity(35);
                write_var_bytes(&mut ser, a.as_bytes());
                self.load(reg, VM_BYTES, &ser);
            }
        }
    }

    fn push(&mut self, reg: u8) {
        self.buf.push(OP_PUSH);
        self.buf.push(reg);
    }

    /// Call `contract.method(args…)`.
    pub fn call_contract(&mut self, contract: &str, method: &str, args: &[Arg<'_>]) -> &mut Self {
        for arg in args.iter().rev() {
            self.load_arg(0, *arg);
            self.push(0);
        }
        self.load(0, VM_STRING, method.as_bytes());
        self.push(0);
        self.load(0, VM_STRING, contract.as_bytes());
        self.buf.extend_from_slice(&[OP_CTX, 0, 1, OP_SWITCH, 1]);
        self
    }

    /// `gas.AllowGas(from, Address.Null, price, limit)`.
    pub fn allow_gas(&mut self, from: Arg<'_>, price: u64, limit: u64) -> &mut Self {
        let (price, limit) = (price.to_string(), limit.to_string());
        self.call_contract("gas", "AllowGas", &[from, Arg::Addr(&Address::NULL), Arg::Str(&price), Arg::Str(&limit)])
    }

    /// `gas.SpendGas(from)`.
    pub fn spend_gas(&mut self, from: Arg<'_>) -> &mut Self {
        self.call_contract("gas", "SpendGas", &[from])
    }

    /// Finish with `RET` and return the script bytes.
    pub fn end(mut self) -> Vec<u8> {
        self.buf.push(OP_RET);
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    #[test]
    fn view_call_matches_the_sdk() {
        // saturnescrow.getState("1") as phantasma-sdk-ts builds it (Saturn kit vector).
        let mut sb = ScriptBuilder::new();
        sb.call_contract("saturnescrow", "getState", &[Arg::Str("1")]);
        assert_eq!(hex(&sb.end()), "0D0004013103000D000408676574537461746503000D00040C73617475726E657363726F772D00012E010B");
    }

    #[test]
    fn varints() {
        let mut v = Vec::new();
        write_varint(&mut v, 0xfc);
        write_varint(&mut v, 0x1d4);
        write_varint(&mut v, 0x1_0000);
        assert_eq!(v, [0xfc, 0xfd, 0xd4, 0x01, 0xfe, 0x00, 0x00, 0x01, 0x00]);
    }
}
