// We cast header -> boxed value a lot in this file.
#![allow(clippy::cast_ptr_alignment)]

use crate::atom;
use crate::bitstring;
use crate::exception;
use crate::immix::Heap;
use crate::loader;
use crate::nanbox::TypedNanBox;
use crate::process::{self, InstrPtr};
use allocator_api::Layout;
use num::bigint::BigInt;
use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

mod closure;
mod cons;
mod map;
mod tuple;
pub use closure::Closure;
pub use cons::Cons;
pub use map::{Map, HAMT};
pub use tuple::Tuple;

pub trait TryInto<T>: Sized {
    /// The type returned in the event of a conversion error.
    type Error;

    /// Performs the conversion.
    fn try_into(&self) -> Result<&T, Self::Error>;
}

#[derive(Debug, PartialEq, PartialOrd, Clone, Copy)]
// annoying: we have to wrap Floats to be able to define hash
pub struct Float(pub f64);
impl Eq for Float {}
impl Hash for Float {
    fn hash<H: Hasher>(&self, _state: &mut H) {
        unimplemented!()
    }
}

const TERM_FLOAT: u8 = 0;
const TERM_NIL: u8 = 1;
const TERM_INTEGER: u8 = 2;
const TERM_ATOM: u8 = 3;
const TERM_PORT: u8 = 4;
const TERM_PID: u8 = 5;
const TERM_CONS: u8 = 6;
const TERM_POINTER: u8 = 7;

pub struct WrongBoxError;

/// A term is a nanboxed compact representation of a value in 64 bits. It can either be immediate,
/// in which case it embeds the data, or a boxed pointer, that points to more data.
#[derive(Debug, Copy, Clone, Eq)]
pub struct Term {
    value: TypedNanBox<Variant>,
}

unsafe impl Sync for Term {}
unsafe impl Send for Term {}

impl Hash for Term {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // maybe we could hash the repr directly
        self.into_variant().hash(state)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Special {
    Nil = 0,
    /// An internal placeholder signifying "THE_NON_VALUE".
    None = 1,
    //Literal,
}

#[derive(Debug, Copy, Clone, Eq, Hash)]
pub enum Variant {
    Float(self::Float),
    Nil(Special), // TODO: expand nil to be able to hold different types of empty (tuple, list, map)
    Integer(i32),
    Atom(u32),
    Port(u32),
    Pid(process::PID),
    Cons(*const self::Cons),
    Pointer(*const Header), // tuple, map, binary, ref
}

impl From<f64> for Term {
    fn from(value: f64) -> Term {
        Term::from(Variant::Float(self::Float(value)))
    }
}

impl From<i32> for Term {
    fn from(value: i32) -> Term {
        Term::from(Variant::Integer(value))
    }
}

impl From<process::PID> for Term {
    fn from(value: process::PID) -> Term {
        Term::from(Variant::Pid(value))
    }
}

impl From<&mut Cons> for Term {
    fn from(value: &mut Cons) -> Term {
        Term::from(Variant::Cons(value))
    }
}

impl From<&mut Tuple> for Term {
    fn from(value: &mut Tuple) -> Term {
        Term::from(Variant::Pointer(value as *const Tuple as *const Header))
    }
}

impl From<&mut Map> for Term {
    fn from(value: &mut Map) -> Term {
        Term::from(Variant::Pointer(value as *const Map as *const Header))
    }
}

impl<T> From<&mut Boxed<T>> for Term {
    fn from(value: &mut Boxed<T>) -> Term {
        Term::from(Variant::Pointer(value as *const Boxed<T> as *const Header))
    }
}

impl From<Variant> for Term {
    fn from(value: Variant) -> Term {
        unsafe {
            match value {
                Variant::Float(self::Float(value)) => Term {
                    value: TypedNanBox::new(TERM_FLOAT, value),
                },
                Variant::Nil(value) => Term {
                    value: TypedNanBox::new(TERM_NIL, value),
                },
                Variant::Integer(value) => Term {
                    value: TypedNanBox::new(TERM_INTEGER, value),
                },
                Variant::Atom(value) => Term {
                    value: TypedNanBox::new(TERM_ATOM, value),
                },
                Variant::Port(value) => Term {
                    value: TypedNanBox::new(TERM_PORT, value),
                },
                Variant::Pid(value) => Term {
                    value: TypedNanBox::new(TERM_PID, value),
                },
                Variant::Cons(value) => Term {
                    value: TypedNanBox::new(TERM_CONS, value),
                },
                Variant::Pointer(value) => Term {
                    value: TypedNanBox::new(TERM_POINTER, value),
                },
            }
        }
    }
}

impl From<Term> for Variant {
    fn from(value: Term) -> Variant {
        value.value.into()
    }
}

impl From<TypedNanBox<Variant>> for Variant {
    fn from(value: TypedNanBox<Variant>) -> Variant {
        #[allow(unused_assignments)]
        unsafe {
            match value.tag() as u8 {
                TERM_FLOAT => Variant::Float(self::Float(value.unpack())),
                TERM_NIL => Variant::Nil(value.unpack()),
                TERM_INTEGER => Variant::Integer(value.unpack()),
                TERM_ATOM => Variant::Atom(value.unpack()),
                TERM_PORT => Variant::Port(value.unpack()),
                TERM_PID => Variant::Pid(value.unpack()),
                TERM_CONS => Variant::Cons(value.unpack()),
                TERM_POINTER => Variant::Pointer(value.unpack()),
                _ => std::hint::unreachable_unchecked(),
            }
        }
    }
}

impl Term {
    pub fn into_variant(self) -> Variant {
        self.into()
    }
}

/// Represents the header of a boxed value on the heap. Is followed by value.
/// Any value allocated on the heap needs repr(C) to guarantee the ordering.
/// This is because we always point to Header, then we recast into the right type.
///
/// TODO: We could avoid this by having the value follow the header and offseting the pointer by
/// header, but that means we'd need to have the header be one full processor word wide to ensure
/// alignment. That means there would be some wasted space.
pub type Header = u8;

pub const BOXED_REF: u8 = 0;
pub const BOXED_TUPLE: u8 = 1;
pub const BOXED_BINARY: u8 = 2;
pub const BOXED_MAP: u8 = 3;
pub const BOXED_BIGINT: u8 = 4;
pub const BOXED_CLOSURE: u8 = 5;
// TODO: these should be direct pointers, no heap
pub const BOXED_CP: u8 = 6;
pub const BOXED_CATCH: u8 = 7;
pub const BOXED_STACKTRACE: u8 = 8;

#[derive(Debug)]
#[repr(C)]
pub struct Boxed<T> {
    pub header: Header,
    pub value: T,
}

/// Strings use an Arc so they can be sent to other processes without
/// requiring a full copy of the data.
#[derive(Debug)]
#[repr(C)]
pub struct Binary {
    pub header: Header,
    pub value: bitstring::Binary,
}

#[derive(Debug)]
#[repr(C)]
pub struct Ref {
    pub header: Header,
    pub value: u32,
}

// term order:
// number < atom < reference < fun < port < pid < tuple < map < nil < list < bit string
#[derive(Eq, PartialEq, Ord, PartialOrd)]
pub enum Type {
    Number,
    Atom,
    Ref,
    Closure,
    Port,
    Pid,
    Tuple,
    Map,
    Nil,
    List,
    Binary,

    // runtime values
    CP,
    Catch,
}

pub enum Num {
    Float(f64),
    Integer(i32),
    Bignum(BigInt),
}

impl Term {
    #[inline(always)]
    pub fn nil() -> Self {
        unsafe {
            Term {
                value: TypedNanBox::new(TERM_NIL, Special::Nil),
            }
        }
    }

    #[inline]
    pub fn none() -> Self {
        unsafe {
            Term {
                value: TypedNanBox::new(TERM_NIL, Special::None),
            }
        }
    }

    #[inline]
    pub fn atom(value: u32) -> Self {
        unsafe {
            Term {
                value: TypedNanBox::new(TERM_ATOM, value),
            }
        }
    }

    // TODO: just use Term::from everywhere
    #[inline]
    pub fn int(value: i32) -> Self {
        unsafe {
            Term {
                value: TypedNanBox::new(TERM_INTEGER, value),
            }
        }
    }

    pub fn pid(value: process::PID) -> Self {
        unsafe {
            Term {
                value: TypedNanBox::new(TERM_PID, value),
            }
        }
    }

    pub fn port(value: u32) -> Self {
        unsafe {
            Term {
                value: TypedNanBox::new(TERM_PORT, value),
            }
        }
    }

    pub fn reference(heap: &Heap, value: u32) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_REF,
            value,
        }))
    }

    pub fn map(heap: &Heap, map: HAMT) -> Self {
        Term::from(heap.alloc(self::Map {
            header: BOXED_MAP,
            map,
        }))
    }

    pub fn closure(heap: &Heap, value: Closure) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_CLOSURE,
            value,
        }))
    }

    pub fn bigint(heap: &Heap, value: BigInt) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_BIGINT,
            value,
        }))
    }

    pub fn binary(heap: &Heap, value: bitstring::Binary) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_BINARY,
            value,
        }))
    }

    pub fn cp(heap: &Heap, value: Option<InstrPtr>) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_CP,
            value,
        }))
    }

    pub fn catch(heap: &Heap, value: InstrPtr) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_CATCH,
            value,
        }))
    }

    pub fn stacktrace(heap: &Heap, value: exception::StackTrace) -> Self {
        Term::from(heap.alloc(Boxed {
            header: BOXED_STACKTRACE,
            value,
        }))
    }

    // immediates

    #[inline(always)]
    pub fn is_none(self) -> bool {
        self.value.tag() as u8 == TERM_NIL // TODO
    }

    #[inline(always)]
    pub fn is_float(self) -> bool {
        self.value.tag() as u8 == TERM_FLOAT
    }

    #[inline(always)]
    pub fn is_nil(self) -> bool {
        self.value.tag() as u8 == TERM_NIL
    }

    #[inline(always)]
    pub fn is_smallint(self) -> bool {
        self.value.tag() as u8 == TERM_INTEGER
    }

    #[inline(always)]
    pub fn is_atom(self) -> bool {
        self.value.tag() as u8 == TERM_ATOM
    }

    #[inline(always)]
    pub fn is_port(self) -> bool {
        self.value.tag() as u8 == TERM_PORT
    }

    #[inline(always)]
    pub fn is_pid(self) -> bool {
        self.value.tag() as u8 == TERM_PID
    }

    #[inline(always)]
    pub fn is_pointer(self) -> bool {
        self.value.tag() as u8 == TERM_POINTER
    }

    #[inline(always)]
    pub fn is_list(self) -> bool {
        let tag = self.value.tag() as u8;
        tag == TERM_CONS || tag == TERM_NIL
    }

    #[inline]
    pub fn get_type(self) -> Type {
        match self.value.tag() as u8 {
            TERM_FLOAT => Type::Number,
            TERM_NIL => Type::Nil,
            TERM_INTEGER => Type::Number,
            TERM_ATOM => Type::Atom,
            TERM_PORT => Type::Port,
            TERM_PID => Type::Pid,
            TERM_CONS => Type::Pid,
            TERM_POINTER => match self.get_boxed_header() {
                BOXED_REF => Type::Ref,
                BOXED_TUPLE => Type::Tuple,
                BOXED_BINARY => Type::Binary,
                BOXED_MAP => Type::Map,
                BOXED_BIGINT => Type::Number,
                BOXED_CLOSURE => Type::Closure,
                BOXED_CP => Type::CP,
                BOXED_CATCH => Type::Catch,
                _ => unimplemented!(),
            },
            _ => unreachable!(),
        }
    }

    pub fn get_boxed_header(self) -> Header {
        if let Variant::Pointer(ptr) = self.into_variant() {
            unsafe { return *ptr }
        }
        panic!("Not a boxed type!")
    }

    pub fn get_boxed_value<T>(&self) -> &T {
        if let Variant::Pointer(ptr) = self.into_variant() {
            unsafe { return &*(ptr as *const T) }
        }
        panic!("Not a boxed type!")
    }

    pub fn get_boxed_value_mut<T>(&self) -> &mut T {
        if let Variant::Pointer(ptr) = self.into_variant() {
            unsafe { return &mut *(ptr as *mut T) }
        }
        panic!("Not a boxed type!")
    }

    /// A method that's optimized for retrieving number types.
    pub fn into_number(self) -> Result<Num, ()> {
        match self.into_variant() {
            Variant::Integer(i) => Ok(Num::Integer(i)),
            Variant::Float(self::Float(i)) => Ok(Num::Float(i)),
            Variant::Pointer(ptr) => unsafe {
                match *ptr {
                    BOXED_BIGINT => {
                        let boxed = &*(ptr as *const Boxed<BigInt>);
                        Ok(Num::Bignum(boxed.value.clone()))
                    }
                    _ => Err(()),
                }
            },
            _ => Err(()),
        }
    }

    // TODO: ExtendedList should instead become a Term vec
    pub fn into_lvalue(self) -> loader::LValue {
        match self.into_variant() {
            Variant::Integer(i) => loader::LValue::Integer(i64::from(i)),
            Variant::Atom(i) => loader::LValue::Atom(i),
            Variant::Nil(..) => loader::LValue::Nil,
            //Variant::Float(self::Float(i)) => Num::Float(i),
            _ => unimplemented!(),
        }
    }

    // ------

    #[inline]
    pub fn is_integer(self) -> bool {
        match self.into_variant() {
            Variant::Integer(_) => true,
            Variant::Pointer(ptr) => unsafe {
                match *ptr {
                    BOXED_BIGINT => true,
                    _ => false,
                }
            },
            _ => false,
        }
    }

    #[inline]
    pub fn is_number(self) -> bool {
        self.get_type() == Type::Number
    }

    #[inline]
    pub fn is_ref(self) -> bool {
        self.get_type() == Type::Ref
    }

    pub fn is_binary(self) -> bool {
        self.get_type() == Type::Binary
    }

    #[inline]
    pub fn is_non_empty_list(self) -> bool {
        match self.into_variant() {
            Variant::Cons(ptr) => unsafe { !(*ptr).head.is_nil() },
            _ => false,
        }
    }

    #[inline]
    pub fn is_tuple(self) -> bool {
        self.get_type() == Type::Tuple
    }

    #[inline]
    pub fn is_function(self) -> bool {
        self.get_type() == Type::Closure
    }

    #[inline]
    pub fn is_boolean(self) -> bool {
        match self.into_variant() {
            Variant::Atom(atom::TRUE) | Variant::Atom(atom::FALSE) => true,
            _ => false,
        }
    }

    #[inline]
    pub fn is_map(self) -> bool {
        self.get_type() == Type::Map
    }

    #[inline]
    pub fn is_cp(self) -> bool {
        self.get_type() == Type::CP
    }

    pub fn to_u32(self) -> u32 {
        match self.into_variant() {
            Variant::Atom(i) => i,
            Variant::Pid(i) => i,
            Variant::Integer(i) => i as u32,
            _ => unimplemented!("to_u32 for {:?}", self),
        }
    }

    pub fn boolean(value: bool) -> Self {
        if value {
            return Variant::Atom(atom::TRUE).into();
        }
        Variant::Atom(atom::FALSE).into()
    }

    pub fn erl_partial_cmp(&self, other: &Self) -> Option<Ordering> {
        // TODO: loosely compare int and floats
        // non strict comparisons need to handle these + bigint
        // (Variant::Integer(_), Variant::Float(_)) => unimplemented!(),
        // (Variant::Float(_), Variant::Integer(_)) => unimplemented!(),
        Some(self.cmp(other))
    }
}

impl PartialEq for Term {
    fn eq(&self, other: &Self) -> bool {
        self.into_variant().eq(&other.into_variant())
    }
}

impl PartialEq for Variant {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Variant::Nil(..), Variant::Nil(..)) => true,
            (Variant::Integer(i1), Variant::Integer(i2)) => i1 == i2,
            (Variant::Float(f1), Variant::Float(f2)) => f1 == f2,

            (Variant::Atom(a1), Variant::Atom(a2)) => a1 == a2,
            (Variant::Pid(p1), Variant::Pid(p2)) => p1 == p2,
            (Variant::Port(p1), Variant::Port(p2)) => p1 == p2,

            (Variant::Cons(l1), Variant::Cons(l2)) => unsafe { (**l1).eq(&(**l2)) },

            (Variant::Pointer(p1), Variant::Pointer(p2)) => unsafe {
                let header = **p1;
                if header == **p2 {
                    match header {
                        BOXED_TUPLE => {
                            let t1 = &*(*p1 as *const Tuple);
                            let t2 = &*(*p2 as *const Tuple);
                            t1.eq(t2)
                        }
                        BOXED_MAP => unimplemented!(),
                        BOXED_CLOSURE => unreachable!(),
                        // TODO: handle other boxed types
                        // ref, bigint, cp, catch, stacktrace
                        _ => unimplemented!(),
                    }
                } else {
                    false
                }
            },
            _ => false,
        }
    }
}

// TODO: make faster by not doing into_variant in some cases
impl PartialOrd for Term {
    fn partial_cmp(&self, other: &Term) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// TODO: make faster by not doing into_variant in some cases
impl Ord for Term {
    fn cmp(&self, other: &Term) -> Ordering {
        // TODO: prevent blowing out the stack from recursion in the future

        // compare types first, if not equal, we can compare them as raw Type casts
        // else, start comparing immediates
        // allow inexact number comparison

        let t1 = self.get_type();
        let t2 = self.get_type();

        if t1 != t2 {
            // types don't match, use term ordering
            return t1.cmp(&t2);
        }

        // types match, let's keep going
        unimplemented!()
    }
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.into_variant())
    }
}

impl std::fmt::Display for Variant {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Variant::Nil(..) => write!(f, "nil"),
            Variant::Integer(i) => write!(f, "{}", i),
            Variant::Float(self::Float(i)) => write!(f, "{}", i),
            Variant::Atom(i) => write!(f, ":{}", atom::to_str(*i).unwrap()),
            Variant::Port(i) => write!(f, "#Port<{}>", i),
            Variant::Pid(i) => write!(f, "#Pid<{}>", i),
            Variant::Cons(c) => unsafe {
                write!(f, "[")?;
                let mut cons = *c;
                loop {
                    write!(f, "{}", (*cons).head)?;
                    match (*cons).tail.into_variant() {
                        // Proper list ends here, do not show the tail
                        Variant::Nil(..) => break,
                        // List continues, print a comma and follow the tail
                        Variant::Cons(c) => {
                            write!(f, ", ")?;
                            cons = c;
                        }
                        // Improper list, show tail
                        val => {
                            write!(f, "| {}", val)?;
                            break;
                        }
                    }
                }
                write!(f, "]")
            },
            Variant::Pointer(ptr) => unsafe {
                match **ptr {
                    BOXED_TUPLE => {
                        let t = &*(*ptr as *const Tuple);

                        write!(f, "{{")?;
                        let mut iter = t.iter().peekable();
                        while let Some(val) = iter.next() {
                            write!(f, "{}", val)?;
                            if iter.peek().is_some() {
                                write!(f, ", ")?;
                            }
                        }
                        write!(f, "}}")
                    }
                    BOXED_REF => write!(f, "#Ref<>"),
                    BOXED_BINARY => write!(f, "#Binary<>"),
                    BOXED_MAP => {
                        let map = &*(*ptr as *const map::Map);
                        write!(f, "%{{")?;
                        let mut iter = map.map.iter().peekable();
                        while let Some((key, val)) = iter.next() {
                            write!(f, "{} => {}", key, val)?;
                            if iter.peek().is_some() {
                                write!(f, ", ")?;
                            }
                        }
                        write!(f, "}}")
                    },
                    BOXED_BIGINT => write!(f, "#BigInt<>"),
                    BOXED_CLOSURE => write!(f, "#Closure<>"),
                    _ => unimplemented!(),
                }
            },
        }
    }
}

#[allow(clippy::mut_from_ref)]
pub fn tuple(heap: &Heap, len: u32) -> &mut Tuple {
    let tuple = heap.alloc(self::Tuple {
        header: BOXED_TUPLE,
        len,
    });
    let layout = Layout::new::<Term>().repeat(len as usize).unwrap().0;
    heap.alloc_layout(layout); // TODO: do something with the ptr
    tuple
}

pub fn cons(heap: &Heap, head: Term, tail: Term) -> Term {
    Term::from(heap.alloc(self::Cons { head, tail }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn test_list_equality() {
        let heap = &Heap::new();
        let v1 = cons!(heap, Term::int(1), cons!(heap, Term::int(2), Term::nil()));
        let v2 = cons!(heap, Term::int(1), cons!(heap, Term::int(2), Term::nil()));
        assert!(v1.eq(&v2));

        let v3 = cons!(heap, Term::int(1), cons!(heap, Term::int(3), Term::nil()));
        assert!(!v1.eq(&v3));
    }

    #[test]
    fn test_tuple_equality() {
        let heap = &Heap::new();
        let v1 = tup2!(heap, Term::int(1), Term::int(2));
        let v2 = tup2!(heap, Term::int(1), Term::int(2));
        assert!(v1.eq(&v2));

        let v3 = tup3!(heap, Term::int(1), Term::int(1), Term::int(1));
        assert!(!v1.eq(&v3));
    }
}
