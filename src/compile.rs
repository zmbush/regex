// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use syntax::{self, Expr, Repeater};

use Error;
use program::{CharRanges, Inst, InstIdx, OneChar};

type Compiled = (Vec<Inst>, Vec<Option<String>>);

/// A regex compiler.
///
/// A regex compiler is responsible for turning a regex's AST into a sequence
/// of instructions.
pub struct Compiler {
    size_limit: usize,
    insts: Vec<Inst>,
    cap_names: Vec<Option<String>>,
}

impl Compiler {
    /// Creates a new compiler that limits the size of the regex program
    /// to the size given (in bytes).
    pub fn new(size_limit: usize) -> Compiler {
        Compiler {
            size_limit: size_limit,
            insts: vec![],
            cap_names: vec![None],
        }
    }

    /// Compiles the given regex AST into a tuple of a sequence of
    /// instructions and a sequence of capture groups, optionally named.
    pub fn compile(mut self, ast: Expr) -> Result<Compiled, Error> {
        self.insts.push(Inst::Save(0));
        try!(self.c(ast));
        self.insts.push(Inst::Save(1));
        self.insts.push(Inst::Match);
        Ok((self.insts, self.cap_names))
    }

    fn c(&mut self, ast: Expr) -> Result<(), Error> {
        use program::Inst::*;
        use program::LookInst::*;

        match ast {
            Expr::Empty => {},
            Expr::Literal { chars, casei } => {
                for mut c in chars {
                    if casei {
                        c = syntax::simple_case_fold(c);
                    }
                    self.push(Char(OneChar { c: c, casei: casei }));
                }
            }
            Expr::AnyChar => self.push(Ranges(CharRanges::any())),
            Expr::AnyCharNoNL => self.push(Ranges(CharRanges::any_nonl())),
            Expr::Class(cls) => {
                if cls.len() == 1 && cls[0].start == cls[0].end {
                    self.push(Char(OneChar {
                        c: cls[0].start,
                        casei: cls.is_case_insensitive(),
                    }));
                } else {
                    self.push(Ranges(CharRanges::from_class(cls)));
                }
            }
            Expr::StartLine => self.push(EmptyLook(StartLine)),
            Expr::EndLine => self.push(EmptyLook(EndLine)),
            Expr::StartText => self.push(EmptyLook(StartText)),
            Expr::EndText => self.push(EmptyLook(EndText)),
            Expr::WordBoundary => self.push(EmptyLook(WordBoundary)),
            Expr::NotWordBoundary => self.push(EmptyLook(NotWordBoundary)),
            Expr::Group { e, i: None, name: None } => try!(self.c(*e)),
            Expr::Group { e, i, name } => {
                let i = i.expect("capture index");
                self.cap_names.push(name);
                self.push(Save(2 * i));
                try!(self.c(*e));
                self.push(Save(2 * i + 1));
            }
            Expr::Concat(es) => {
                for e in es {
                    try!(self.c(e));
                }
            }
            Expr::Alternate(mut es) => {
                // TODO: Don't use recursion here. ---AG
                if es.len() == 0 {
                    return Ok(());
                }
                let e1 = es.remove(0);
                if es.len() == 0 {
                    try!(self.c(e1));
                    return Ok(());
                }
                let e2 = Expr::Alternate(es); // this causes recursion

                let split = self.empty_split();
                let j1 = self.insts.len();
                try!(self.c(e1));
                let jmp = self.empty_jump();
                let j2 = self.insts.len();
                try!(self.c(e2));
                let j3 = self.insts.len();

                self.set_split(split, j1, j2);
                self.set_jump(jmp, j3);
            }
            Expr::Repeat { e, r: Repeater::ZeroOrOne, greedy } => {
                let split = self.empty_split();
                let j1 = self.insts.len();
                try!(self.c(*e));
                let j2 = self.insts.len();

                if greedy {
                    self.set_split(split, j1, j2);
                } else {
                    self.set_split(split, j2, j1);
                }
            }
            Expr::Repeat { e, r: Repeater::ZeroOrMore, greedy } => {
                let j1 = self.insts.len();
                let split = self.empty_split();
                let j2 = self.insts.len();
                try!(self.c(*e));
                let jmp = self.empty_jump();
                let j3 = self.insts.len();

                self.set_jump(jmp, j1);
                if greedy {
                    self.set_split(split, j2, j3);
                } else {
                    self.set_split(split, j3, j2);
                }
            }
            Expr::Repeat { e, r: Repeater::OneOrMore, greedy } => {
                let j1 = self.insts.len();
                try!(self.c(*e));
                let split = self.empty_split();
                let j2 = self.insts.len();

                if greedy {
                    self.set_split(split, j1, j2);
                } else {
                    self.set_split(split, j2, j1);
                }
            }
            Expr::Repeat {
                e,
                r: Repeater::Range { min, max: None },
                greedy,
            } => {
                let e = *e;
                for _ in 0..min {
                    try!(self.c(e.clone()));
                }
                try!(self.c(Expr::Repeat {
                    e: Box::new(e),
                    r: Repeater::ZeroOrMore,
                    greedy: greedy,
                }));
            }
            Expr::Repeat {
                e,
                r: Repeater::Range { min, max: Some(max) },
                greedy,
            } => {
                let e = *e;
                for _ in 0..min {
                    try!(self.c(e.clone()));
                }
                for _ in min..max {
                    try!(self.c(Expr::Repeat {
                        e: Box::new(e.clone()),
                        r: Repeater::ZeroOrOne,
                        greedy: greedy,
                    }));
                }
            }
        }
        self.check_size()
    }

    fn check_size(&self) -> Result<(), Error> {
        use std::mem::size_of;

        if self.insts.len() * size_of::<Inst>() > self.size_limit {
            Err(Error::CompiledTooBig(self.size_limit))
        } else {
            Ok(())
        }
    }

    /// Appends the given instruction to the program.
    #[inline]
    fn push(&mut self, x: Inst) {
        self.insts.push(x)
    }

    /// Appends an *empty* `Split` instruction to the program and returns
    /// the index of that instruction. (The index can then be used to "patch"
    /// the actual locations of the split in later.)
    #[inline]
    fn empty_split(&mut self) -> InstIdx {
        self.insts.push(Inst::Split(0, 0));
        self.insts.len() - 1
    }

    /// Sets the left and right locations of a `Split` instruction at index
    /// `i` to `pc1` and `pc2`, respectively.
    /// If the instruction at index `i` isn't a `Split` instruction, then
    /// `panic!` is called.
    #[inline]
    fn set_split(&mut self, i: InstIdx, pc1: InstIdx, pc2: InstIdx) {
        let split = &mut self.insts[i];
        match *split {
            Inst::Split(_, _) => *split = Inst::Split(pc1, pc2),
            _ => panic!("BUG: Invalid split index."),
        }
    }

    /// Appends an *empty* `Jump` instruction to the program and returns the
    /// index of that instruction.
    #[inline]
    fn empty_jump(&mut self) -> InstIdx {
        self.insts.push(Inst::Jump(0));
        self.insts.len() - 1
    }

    /// Sets the location of a `Jump` instruction at index `i` to `pc`.
    /// If the instruction at index `i` isn't a `Jump` instruction, then
    /// `panic!` is called.
    #[inline]
    fn set_jump(&mut self, i: InstIdx, pc: InstIdx) {
        let jmp = &mut self.insts[i];
        match *jmp {
            Inst::Jump(_) => *jmp = Inst::Jump(pc),
            _ => panic!("BUG: Invalid jump index."),
        }
    }
}
