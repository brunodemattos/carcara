//! A parser for the Alethe Proof Format.

pub mod error;
pub mod lexer;
pub mod tests;

use crate::{
    ast::*,
    utils::{Either, SymbolTable},
    AletheResult, Error,
};
use ahash::{AHashMap, AHashSet};
use error::*;
use lexer::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::ToPrimitive;
use std::{io::BufRead, str::FromStr};

pub fn parse_instance<T: BufRead>(problem: T, proof: T) -> AletheResult<(Proof, TermPool)> {
    let mut problem_parser = Parser::new(problem)?;
    let premises = problem_parser.parse_problem()?;
    let mut proof_parser = Parser::with_state(proof, problem_parser.state)?;

    let commands = proof_parser.parse_proof()?;
    let proof = Proof { premises, commands };
    Ok((proof, proof_parser.state.term_pool))
}

type AnchorCommand = (String, Vec<(String, Rc<Term>)>, Vec<SortedVar>);
type StepCommand = (
    Vec<Rc<Term>>, // Clause
    String,        // Rule
    Vec<String>,   // Premises
    Vec<ProofArg>, // Arguments
    Vec<String>,   // Discharge
);
#[derive(Default)]
pub(crate) struct ParserState {
    sorts_symbol_table: SymbolTable<Identifier, Rc<Term>>,
    function_defs: AHashMap<String, FunctionDef>,
    pub(crate) term_pool: TermPool,
    sort_declarations: AHashMap<String, usize>,
    step_indices: SymbolTable<String, usize>,
}

/// A parser for the Alethe Proof Format. The parser makes use of hash consing to reduce memory usage
/// by sharing identical terms in the AST.
pub struct Parser<R> {
    lexer: Lexer<R>,
    current_token: Token,
    current_position: Position,
    state: ParserState,
    interpret_integers_as_reals: bool,
}

impl<R: BufRead> Parser<R> {
    /// Constructs a new `Parser` from a type that implements `BufRead`. This operation can fail if
    /// there is an IO or lexer error on the first token.
    pub fn new(input: R) -> AletheResult<Self> {
        let mut state = ParserState::default();
        let bool_sort = state.term_pool.add_term(Term::Sort(Sort::Bool));
        for iden in ["true", "false"] {
            let iden = Identifier::Simple(iden.to_string());
            state.sorts_symbol_table.insert(iden, bool_sort.clone());
        }
        Parser::with_state(input, state)
    }

    /// Constructs a new `Parser` using an existing `ParserState`. This operation can fail if there
    /// is an IO or lexer error on the first token.
    fn with_state(input: R, state: ParserState) -> AletheResult<Self> {
        let mut lexer = Lexer::new(input)?;
        let (current_token, current_position) = lexer.next_token()?;
        Ok(Parser {
            lexer,
            current_token,
            current_position,
            state,
            interpret_integers_as_reals: false,
        })
    }

    /// Advances the parser one token, and returns the previous `current_token`.
    fn next_token(&mut self) -> AletheResult<(Token, Position)> {
        use std::mem::replace;

        let (new_token, new_position) = self.lexer.next_token()?;
        let old_token = replace(&mut self.current_token, new_token);
        let old_position = replace(&mut self.current_position, new_position);
        Ok((old_token, old_position))
    }

    /// Shortcut for `self.state.term_pool.add_term`.
    fn add_term(&mut self, term: Term) -> Rc<Term> {
        self.state.term_pool.add_term(term)
    }

    /// Shortcut for `self.state.term_pool.add_all`.
    fn add_all(&mut self, term: Vec<Term>) -> Vec<Rc<Term>> {
        self.state.term_pool.add_all(term)
    }

    /// Helper method to insert a `SortedVar` into the parser symbol table.
    fn insert_sorted_var(&mut self, (symbol, sort): SortedVar) {
        self.state
            .sorts_symbol_table
            .insert(Identifier::Simple(symbol), sort)
    }

    /// Constructs and sort checks a variable term.
    fn make_var(&mut self, iden: Identifier) -> Result<Rc<Term>, ParserError> {
        let sort = self
            .state
            .sorts_symbol_table
            .get(&iden)
            .ok_or_else(|| ParserError::UndefinedIden(iden.clone()))?
            .clone();
        Ok(self.add_term(Term::Terminal(Terminal::Var(iden, sort))))
    }

    /// Constructs and sort checks an operation term.
    fn make_op(&mut self, op: Operator, args: Vec<Rc<Term>>) -> Result<Rc<Term>, ParserError> {
        let sorts: Vec<_> = args.iter().map(|t| t.sort()).collect();
        match op {
            Operator::Not => {
                ParserError::assert_num_of_args(&args, 1)?;
                SortError::assert_eq(&Sort::Bool, sorts[0])?;
            }
            Operator::Implies => {
                ParserError::assert_num_of_args_range(&args, 2..)?;
                for s in sorts {
                    SortError::assert_eq(&Sort::Bool, s)?;
                }
            }
            Operator::Or | Operator::And | Operator::Xor => {
                // These operators can be called with only one argument
                ParserError::assert_num_of_args_range(&args, 1..)?;
                for s in sorts {
                    SortError::assert_eq(&Sort::Bool, s)?;
                }
            }
            Operator::Equals | Operator::Distinct => {
                ParserError::assert_num_of_args_range(&args, 2..)?;
                SortError::assert_all_eq(&sorts)?;
            }
            Operator::Ite => {
                ParserError::assert_num_of_args(&args, 3)?;
                SortError::assert_eq(&Sort::Bool, sorts[0])?;
                SortError::assert_eq(sorts[1], sorts[2])?;
            }
            Operator::Add | Operator::Mult | Operator::IntDiv | Operator::RealDiv => {
                ParserError::assert_num_of_args_range(&args, 2..)?;

                // All the arguments must have the same sort, and it must be either Int or Real
                SortError::assert_one_of(&[Sort::Int, Sort::Real], sorts[0])?;
                SortError::assert_all_eq(&sorts)?;
            }
            Operator::Sub => {
                // The "-" operator, in particular, can be called with only one argument, in which
                // case it means negation instead of subtraction
                ParserError::assert_num_of_args_range(&args, 1..)?;
                SortError::assert_one_of(&[Sort::Int, Sort::Real], sorts[0])?;
                SortError::assert_all_eq(&sorts)?;
            }
            Operator::LessThan | Operator::GreaterThan | Operator::LessEq | Operator::GreaterEq => {
                ParserError::assert_num_of_args_range(&args, 2..)?;
                // All the arguments must be either Int or Real sorted, but they don't need to all
                // have the same sort
                for s in sorts {
                    SortError::assert_one_of(&[Sort::Int, Sort::Real], s)?;
                }
            }
            Operator::Select => {
                ParserError::assert_num_of_args(&args, 2)?;
                match sorts[0] {
                    Sort::Array(_, _) => (),
                    got => {
                        // Instead of creating some special case for sort errors with parametric
                        // sorts, we just create a sort "Y" to represent the sort parameter. We
                        // infer the "X" sort from the second operator argument. This may be
                        // changed later
                        let x = self.add_term(Term::Sort(sorts[1].clone()));
                        let y = self.add_term(Term::Sort(Sort::Atom("Y".to_string(), Vec::new())));
                        return Err(SortError {
                            expected: vec![Sort::Array(x, y)],
                            got: got.clone(),
                        }
                        .into());
                    }
                }
            }
            Operator::Store => {
                ParserError::assert_num_of_args(&args, 3)?;
                match sorts[0] {
                    Sort::Array(x, y) => {
                        SortError::assert_eq(x.as_sort().unwrap(), sorts[1])?;
                        SortError::assert_eq(y.as_sort().unwrap(), sorts[2])?;
                    }
                    got => {
                        return Err(SortError {
                            expected: vec![Sort::Array(
                                self.add_term(Term::Sort(sorts[0].clone())),
                                self.add_term(Term::Sort(sorts[1].clone())),
                            )],
                            got: got.clone(),
                        }
                        .into());
                    }
                }
            }
        }
        Ok(self.add_term(Term::Op(op, args)))
    }

    /// Constructs and sort checks an application term.
    fn make_app(
        &mut self,
        function: Rc<Term>,
        args: Vec<Rc<Term>>,
    ) -> Result<Rc<Term>, ParserError> {
        let sorts = {
            let function_sort = function.sort();
            if let Sort::Function(sorts) = function_sort {
                sorts
            } else {
                // Function does not have function sort
                return Err(ParserError::NotAFunction(function_sort.clone()));
            }
        };
        ParserError::assert_num_of_args(&args, sorts.len() - 1)?;
        for i in 0..args.len() {
            SortError::assert_eq(sorts[i].as_sort().unwrap(), args[i].sort())?;
        }
        Ok(self.add_term(Term::App(function, args)))
    }

    /// Consumes the current token if it equals `expected`. Returns an error otherwise.
    fn expect_token(&mut self, expected: Token) -> AletheResult<()> {
        let (got, pos) = self.next_token()?;
        if got == expected {
            Ok(())
        } else {
            Err(Error::Parser(ParserError::UnexpectedToken(got), pos))
        }
    }

    /// Consumes the current token if it is a symbol, and returns the inner `String`. Returns an
    /// error otherwise.
    fn expect_symbol(&mut self) -> AletheResult<String> {
        match self.next_token()? {
            (Token::Symbol(s), _) => Ok(s),
            (other, pos) => Err(Error::Parser(ParserError::UnexpectedToken(other), pos)),
        }
    }

    /// Consumes the current token if it is a keyword, and returns the inner `String`. Returns an
    /// error otherwise.
    fn expect_keyword(&mut self) -> AletheResult<String> {
        match self.next_token()? {
            (Token::Keyword(s), _) => Ok(s),
            (other, pos) => Err(Error::Parser(ParserError::UnexpectedToken(other), pos)),
        }
    }

    /// Consumes the current token if it is a numeral, and returns the inner `BigInt`. Returns an
    /// error otherwise.
    fn expect_numeral(&mut self) -> AletheResult<BigInt> {
        match self.next_token()? {
            (Token::Numeral(n), _) => Ok(n),
            (other, pos) => Err(Error::Parser(ParserError::UnexpectedToken(other), pos)),
        }
    }

    /// Calls `parse_func` repeatedly until a closing parenthesis is reached. If `non_empty` is
    /// true, empty sequences will result in an error. This method consumes the ending ")" token.
    fn parse_sequence<T, F>(&mut self, mut parse_func: F, non_empty: bool) -> AletheResult<Vec<T>>
    where
        F: FnMut(&mut Self) -> AletheResult<T>,
    {
        let mut result = Vec::new();
        while self.current_token != Token::CloseParen {
            result.push(parse_func(self)?);
        }
        if non_empty && result.is_empty() {
            Err(Error::Parser(
                ParserError::EmptySequence,
                self.current_position,
            ))
        } else {
            self.next_token()?; // Consume ")" token
            Ok(result)
        }
    }

    fn read_until_close_parens(&mut self) -> AletheResult<()> {
        let mut parens_depth = 1;
        while parens_depth > 0 {
            parens_depth += match self.next_token()? {
                (Token::OpenParen, _) => 1,
                (Token::CloseParen, _) => -1,
                (Token::Eof, pos) => {
                    return Err(Error::Parser(ParserError::UnexpectedToken(Token::Eof), pos))
                }
                _ => 0,
            };
        }
        Ok(())
    }

    /// Reads an SMT-LIB script and parses the declarations and definitions. Ignores all other
    /// SMT-LIB script commands.
    pub fn parse_problem(&mut self) -> AletheResult<AHashSet<Rc<Term>>> {
        let mut premises = AHashSet::new();

        while self.current_token != Token::Eof {
            self.expect_token(Token::OpenParen)?;
            match self.next_token()?.0 {
                Token::ReservedWord(Reserved::DeclareFun) => {
                    let (name, sort) = self.parse_declare_fun()?;
                    self.insert_sorted_var((name, sort));
                    continue;
                }
                Token::ReservedWord(Reserved::DeclareConst) => {
                    let name = self.expect_symbol()?;
                    let sort = self.parse_sort()?;
                    let sort = self.add_term(sort);
                    self.expect_token(Token::CloseParen)?;
                    self.insert_sorted_var((name, sort));
                    continue;
                }
                Token::ReservedWord(Reserved::DeclareSort) => {
                    let (name, arity) = self.parse_declare_sort()?;
                    // User declared sorts are represented with the `Atom` sort kind, and an
                    // argument which is a string terminal representing the sort name.
                    self.state.sort_declarations.insert(name, arity);
                    continue;
                }
                Token::ReservedWord(Reserved::DefineFun) => {
                    let (name, func_def) = self.parse_define_fun()?;
                    self.state.function_defs.insert(name, func_def);
                    continue;
                }
                Token::ReservedWord(Reserved::Assert) => {
                    let term = self.parse_term()?;
                    self.expect_token(Token::CloseParen)?;
                    premises.insert(term);
                }
                Token::Symbol(s) if s == "set-logic" => {
                    let logic = self.expect_symbol()?;

                    // When the problem's logic contains real numbers but not integers, integer
                    // literals should be parsed as reals. For instance, "1" should be interpreted
                    // as "1.0".
                    self.interpret_integers_as_reals = match logic.as_str() {
                        "LRA" | "QF_LRA" | "QF_NRA" | "QF_RDL" | "QF_UFLRA" | "QF_UFNRA"
                        | "UFLRA" => true,

                        "AUFLIA" | "AUFLIRA" | "AUFNIRA" | "LIA" | "QF_ABV" | "QF_AUFBV"
                        | "QF_AUFLIA" | "QF_AX" | "QF_BV" | "QF_IDL" | "QF_LIA" | "QF_NIA"
                        | "QF_UF" | "QF_UFBV" | "QF_UFIDL" | "QF_UFLIA" | "UFNIA" => false,

                        other => {
                            log::warn!("unknown logic: {}", other);
                            false
                        }
                    };

                    self.expect_token(Token::CloseParen)?;
                }
                _ => {
                    // If the command is not one of the commands we care about, we just ignore it.
                    // We do that by reading tokens until the command parenthesis is closed
                    self.read_until_close_parens()?;
                }
            }
        }
        Ok(premises)
    }

    /// Parses a proof.
    pub fn parse_proof(&mut self) -> AletheResult<Vec<ProofCommand>> {
        // To avoid stack overflows in proofs with many nested subproofs, we parse the subproofs
        // iteratively, instead of recursively
        let mut commands_stack = vec![Vec::new()];
        let mut end_step_stack = Vec::new();
        let mut subproof_args_stack = Vec::new();

        while self.current_token != Token::Eof {
            self.expect_token(Token::OpenParen)?;
            let (token, position) = self.next_token()?;
            let (index, command) = match token {
                Token::ReservedWord(Reserved::Assume) => {
                    let (index, term) = self.parse_assume_command()?;
                    (index.clone(), ProofCommand::Assume { index, term })
                }
                Token::ReservedWord(Reserved::Step) => {
                    let (index, (clause, rule, premises, args, discharge)) =
                        self.parse_step_command()?;

                    // For every premise index symbol, find the associated premise index (depth and
                    // command index) in the `step_indices` symbol table, or return an error
                    let premises: Vec<_> = premises
                        .into_iter()
                        .map(|index| {
                            self.state
                                .step_indices
                                .get_with_depth(&index)
                                .map(|(d, &i)| (d, i))
                                .ok_or(Error::Parser(
                                    ParserError::UndefinedStepIndex(index),
                                    // TODO: Make this error carry the position of the actual
                                    // premise token
                                    position,
                                ))
                        })
                        .collect::<Result<_, _>>()?;

                    let step = ProofStep {
                        index: index.clone(),
                        clause,
                        rule,
                        premises,
                        args,
                        discharge,
                    };
                    (index, ProofCommand::Step(step))
                }
                Token::ReservedWord(Reserved::DefineFun) => {
                    let (name, func_def) = self.parse_define_fun()?;
                    self.state.function_defs.insert(name, func_def);
                    continue;
                }
                Token::ReservedWord(Reserved::Anchor) => {
                    let (end_step_index, assignment_args, variable_args) =
                        self.parse_anchor_command()?;

                    // When we encounter an "anchor" command, we push a new scope into the step
                    // indices symbol table, a fresh commands vector into the commands stack for
                    // the subproof to fill, and the "anchor" data (end step and arguments) into
                    // their respective stacks. All of this will be popped off at the end of the
                    // subproof. We don't need to push a new scope into the sorts symbol table
                    // because `Parser::parse_anchor_command` already does that for us
                    self.state.step_indices.push_scope();
                    commands_stack.push(Vec::new());
                    end_step_stack.push(end_step_index);
                    subproof_args_stack.push((assignment_args, variable_args));
                    continue;
                }
                _ => return Err(Error::Parser(ParserError::UnexpectedToken(token), position)),
            };
            if self.state.step_indices.get(&index).is_some() {
                return Err(Error::Parser(
                    ParserError::RepeatedStepIndex(index),
                    position,
                ));
            }

            commands_stack.last_mut().unwrap().push(command);
            if end_step_stack.last() == Some(&index) {
                // If this is the last step in a subproof, we need to pop all the subproof data off
                // of the stacks and build the subproof command with it
                self.state.sorts_symbol_table.pop_scope();
                self.state.step_indices.pop_scope();
                let commands = commands_stack.pop().unwrap();
                end_step_stack.pop().unwrap();
                let (assignment_args, variable_args) = subproof_args_stack.pop().unwrap();

                // We also need to make sure that the last command is in fact a "step"
                match commands.last() {
                    Some(ProofCommand::Step(_)) => (),
                    _ => {
                        return Err(Error::Parser(
                            ParserError::LastSubproofStepIsNotStep(index),
                            position,
                        ))
                    }
                };

                commands_stack
                    .last_mut()
                    .unwrap()
                    .push(ProofCommand::Subproof {
                        commands,
                        assignment_args,
                        variable_args,
                    })
            }
            self.state
                .step_indices
                .insert(index, commands_stack.last().unwrap().len() - 1);
        }
        match commands_stack.len() {
            0 => unreachable!(),
            1 => Ok(commands_stack.pop().unwrap()),

            // If there is more than one vector in the commands stack, we are inside a subproof
            // that should be closed before the outer proof is finished
            _ => Err(Error::Parser(
                ParserError::UnclosedSubproof(end_step_stack.pop().unwrap()),
                self.current_position,
            )),
        }
    }

    /// Parses an "assume" proof command. This method assumes that the "(" and "assume" tokens were
    /// already consumed.
    fn parse_assume_command(&mut self) -> AletheResult<(String, Rc<Term>)> {
        let index = self.expect_symbol()?;
        let term = self.parse_term_expecting_sort(&Sort::Bool)?;
        self.expect_token(Token::CloseParen)?;
        Ok((index, term))
    }

    /// Parses a "step" proof command. This method assumes that the "(" and "step" tokens were
    /// already consumed.
    fn parse_step_command(&mut self) -> AletheResult<(String, StepCommand)> {
        let step_index = self.expect_symbol()?;
        let clause = self.parse_clause()?;
        self.expect_token(Token::Keyword("rule".into()))?;
        let rule = match self.next_token()? {
            (Token::Symbol(s), _) => s,
            (Token::ReservedWord(r), _) => format!("{}", r),
            (other, pos) => return Err(Error::Parser(ParserError::UnexpectedToken(other), pos)),
        };

        // If the rule is "trust", we read the rest of the "step" command, ignoring all arguments
        // and premises
        if rule == "trust" {
            self.read_until_close_parens()?;
            return Ok((
                step_index,
                (clause, rule, Vec::new(), Vec::new(), Vec::new()),
            ));
        }

        let premises = if self.current_token == Token::Keyword("premises".into()) {
            self.next_token()?;
            self.expect_token(Token::OpenParen)?;
            self.parse_sequence(Self::expect_symbol, true)?
        } else {
            Vec::new()
        };

        let args = if self.current_token == Token::Keyword("args".into()) {
            self.next_token()?;
            self.expect_token(Token::OpenParen)?;
            self.parse_sequence(Self::parse_proof_arg, true)?
        } else {
            Vec::new()
        };

        // In some steps (notably those with the "subproof" rule) a ":discharge" attribute appears,
        // with a sequence of assumption indices as its value. While the checker already has
        // support this rule, it doesn't use these values to check it. These values are only used
        // when printing a proof.
        let discharge = if self.current_token == Token::Keyword("discharge".into()) {
            self.next_token()?;
            self.expect_token(Token::OpenParen)?;
            self.parse_sequence(Self::expect_symbol, true)?
        } else {
            Vec::new()
        };

        self.expect_token(Token::CloseParen)?;

        Ok((step_index, (clause, rule, premises, args, discharge)))
    }

    /// Parses an "anchor" proof command. This method assumes that the "(" and "anchor" tokens were
    /// already consumed. In order to parse the subproof arguments, this method pushes a new scope
    /// into the sorts symbol table which must be removed after parsing the subproof. This method
    /// returns the index of the step that will end the subproof, as well as the subproof
    /// assignment and variable arguments.
    fn parse_anchor_command(&mut self) -> AletheResult<AnchorCommand> {
        self.expect_token(Token::Keyword("step".into()))?;
        let end_step_index = self.expect_symbol()?;

        // We have to push a new scope into the sorts symbol table in order to parse the subproof
        // arguments
        self.state.sorts_symbol_table.push_scope();

        let mut assignment_args = Vec::new();
        let mut variable_args = Vec::new();
        if self.current_token == Token::Keyword("args".into()) {
            self.next_token()?;
            self.expect_token(Token::OpenParen)?;
            let args = self.parse_sequence(Parser::parse_anchor_argument, true)?;
            for a in args {
                match a {
                    Either::Left(((a, _), b)) => {
                        assignment_args.push((a.clone(), b));
                    }
                    Either::Right(var) => variable_args.push(var.clone()),
                }
            }
        }
        self.expect_token(Token::CloseParen)?;
        Ok((end_step_index, assignment_args, variable_args))
    }

    fn parse_anchor_argument(&mut self) -> AletheResult<Either<(SortedVar, Rc<Term>), SortedVar>> {
        self.expect_token(Token::OpenParen)?;
        Ok(if self.current_token == Token::Keyword("=".into()) {
            self.next_token()?;
            let (a, sort) = self.parse_sorted_var()?;
            self.insert_sorted_var((a.clone(), sort.clone()));

            let b = match &self.current_token {
                // If we encounter a symbol as the value of the assignment, we must check if there
                // are any function definitions with that symbol. If there are, we consider the
                // value a term instead of a new variable
                Token::Symbol(s) if !self.state.function_defs.contains_key(s) => {
                    let var = self.expect_symbol()?;
                    self.insert_sorted_var((var.clone(), sort.clone()));
                    let iden = Identifier::Simple(var);
                    self.add_term(Term::Terminal(Terminal::Var(iden, sort.clone())))
                }
                _ => self.parse_term_expecting_sort(sort.as_sort().unwrap())?,
            };

            self.expect_token(Token::CloseParen)?;
            Either::Left(((a, sort), b))
        } else {
            let symbol = self.expect_symbol()?;
            let sort = self.parse_sort()?;
            let var = (symbol, self.add_term(sort));
            self.insert_sorted_var(var.clone());
            self.expect_token(Token::CloseParen)?;
            Either::Right(var)
        })
    }

    /// Parses a "declare-fun" proof command. Returns the function name and a term representing its
    /// sort. This method assumes that the "(" and "declare-fun" tokens were already consumed.
    fn parse_declare_fun(&mut self) -> AletheResult<(String, Rc<Term>)> {
        let name = self.expect_symbol()?;
        let sort = {
            self.expect_token(Token::OpenParen)?;
            let mut sorts = self.parse_sequence(Self::parse_sort, false)?;
            sorts.push(self.parse_sort()?);
            let sorts = self.add_all(sorts);
            if sorts.len() == 1 {
                sorts.into_iter().next().unwrap()
            } else {
                self.add_term(Term::Sort(Sort::Function(sorts)))
            }
        };
        self.expect_token(Token::CloseParen)?;
        Ok((name, sort))
    }

    /// Parses a declare-sort proof command. Returns the sort name and its arity. This method
    /// assumes that the "(" and "declare-sort" tokens were already consumed.
    fn parse_declare_sort(&mut self) -> AletheResult<(String, usize)> {
        let name = self.expect_symbol()?;
        let arity_pos = self.current_position;
        let arity = self.expect_numeral()?;
        self.expect_token(Token::CloseParen)?;
        let arity = arity.to_usize().ok_or(Error::Parser(
            ParserError::InvalidSortArity(arity),
            arity_pos,
        ))?;
        Ok((name, arity))
    }

    /// Parses a "define-fun" proof command. Returns the function name and its definition. This
    /// method assumes that the "(" and "define-fun" tokens were already consumed.
    fn parse_define_fun(&mut self) -> AletheResult<(String, FunctionDef)> {
        let name = self.expect_symbol()?;
        self.expect_token(Token::OpenParen)?;
        let params = self.parse_sequence(Self::parse_sorted_var, false)?;
        let return_sort = self.parse_sort()?;

        // In order to correctly parse the function body, we push a new scope to the symbol table
        // and add the functions arguments to it.
        self.state.sorts_symbol_table.push_scope();
        for var in &params {
            self.insert_sorted_var(var.clone());
        }
        let body = self.parse_term_expecting_sort(return_sort.as_sort().unwrap())?;
        self.state.sorts_symbol_table.pop_scope();

        self.expect_token(Token::CloseParen)?;

        Ok((name, FunctionDef { params, body }))
    }

    /// Parses a clause of the form "(cl <term>*)".
    fn parse_clause(&mut self) -> AletheResult<Vec<Rc<Term>>> {
        self.expect_token(Token::OpenParen)?;
        self.expect_token(Token::ReservedWord(Reserved::Cl))?;
        self.parse_sequence(|p| p.parse_term_expecting_sort(&Sort::Bool), false)
    }

    /// Parses an argument for a "step" command.
    fn parse_proof_arg(&mut self) -> AletheResult<ProofArg> {
        if self.current_token == Token::OpenParen {
            self.next_token()?; // Consume "(" token

            // If we encounter a "(" token, this could be an assignment argument of the form
            // "(:= <symbol> <term>)", or a regular term that starts with "(". Note that the
            // lexer reads ":=" as a keyword with contents "=".
            if self.current_token == Token::Keyword("=".into()) {
                self.next_token()?; // Consume ":=" token
                let name = self.expect_symbol()?;
                let value = self.parse_term()?;
                self.expect_token(Token::CloseParen)?;
                Ok(ProofArg::Assign(name, value))
            } else {
                // If the first token is not ":=", this argument is just a regular term. Since
                // we already consumed the "(" token, we have to call `parse_application`
                // instead of `parse_term`.
                let term = self.parse_application()?;
                Ok(ProofArg::Term(term))
            }
        } else {
            let term = self.parse_term()?;
            Ok(ProofArg::Term(term))
        }
    }

    /// Parses a sorted variable of the form "(<symbol> <sort>)".
    fn parse_sorted_var(&mut self) -> AletheResult<SortedVar> {
        self.expect_token(Token::OpenParen)?;
        let symbol = self.expect_symbol()?;
        let sort = self.parse_sort()?;
        self.expect_token(Token::CloseParen)?;
        Ok((symbol, self.add_term(sort)))
    }

    /// Parses a term.
    pub fn parse_term(&mut self) -> AletheResult<Rc<Term>> {
        let term = match self.next_token()? {
            (Token::Numeral(n), _) if self.interpret_integers_as_reals => {
                terminal!(real BigRational::from_integer(n))
            }
            (Token::Numeral(n), _) => terminal!(int n),
            (Token::Decimal(r), _) => terminal!(real r),
            (Token::String(s), _) => terminal!(string s),
            (Token::Symbol(s), pos) => {
                // Check to see if there is a nullary function defined with this name
                return Ok(if let Some(func_def) = self.state.function_defs.get(&s) {
                    if func_def.params.is_empty() {
                        // This has to clone the function body term, even though it is already
                        // added to the term pool
                        func_def.body.clone()
                    } else {
                        return Err(Error::Parser(
                            ParserError::WrongNumberOfArgs(func_def.params.len(), 0),
                            pos,
                        ));
                    }
                } else {
                    self.make_var(Identifier::Simple(s))
                        .map_err(|err| Error::Parser(err, pos))?
                });
            }
            (Token::OpenParen, _) => return self.parse_application(),
            (other, pos) => return Err(Error::Parser(ParserError::UnexpectedToken(other), pos)),
        };
        Ok(self.add_term(term))
    }

    /// Parses a term and checks that its sort matches the expected sort. If not, returns an error.
    fn parse_term_expecting_sort(&mut self, expected_sort: &Sort) -> AletheResult<Rc<Term>> {
        let pos = self.current_position;
        let term = self.parse_term()?;
        SortError::assert_eq(expected_sort, term.sort())
            .map_err(|e| Error::Parser(e.into(), pos))?;
        Ok(term)
    }

    fn parse_quantifier(&mut self, quantifier: Quantifier) -> AletheResult<Rc<Term>> {
        self.expect_token(Token::OpenParen)?;
        self.state.sorts_symbol_table.push_scope();
        let bindings = self.parse_sequence(
            |p| {
                let var = p.parse_sorted_var()?;
                p.insert_sorted_var(var.clone());
                Ok(var)
            },
            true,
        )?;
        let term = self.parse_term_expecting_sort(&Sort::Bool)?;
        self.state.sorts_symbol_table.pop_scope();
        self.expect_token(Token::CloseParen)?;
        Ok(self.add_term(Term::Quant(quantifier, BindingList(bindings), term)))
    }

    fn parse_choice_term(&mut self) -> AletheResult<Rc<Term>> {
        self.expect_token(Token::OpenParen)?;
        let var = self.parse_sorted_var()?;
        self.insert_sorted_var(var.clone());
        self.expect_token(Token::CloseParen)?;
        let inner = self.parse_term()?;
        self.expect_token(Token::CloseParen)?;
        Ok(self.add_term(Term::Choice(var, inner)))
    }

    fn parse_let_term(&mut self) -> AletheResult<Rc<Term>> {
        self.expect_token(Token::OpenParen)?;
        self.state.sorts_symbol_table.push_scope();
        let bindings = self.parse_sequence(
            |p| {
                p.expect_token(Token::OpenParen)?;
                let name = p.expect_symbol()?;
                let value = p.parse_term()?;
                let sort = p.add_term(Term::Sort(value.sort().clone()));
                p.insert_sorted_var((name.clone(), sort));
                p.expect_token(Token::CloseParen)?;
                Ok((name, value))
            },
            true,
        )?;
        let inner = self.parse_term()?;
        self.expect_token(Token::CloseParen)?;
        self.state.sorts_symbol_table.pop_scope();
        Ok(self.add_term(Term::Let(BindingList(bindings), inner)))
    }

    fn parse_annotated_term(&mut self) -> AletheResult<Rc<Term>> {
        let inner = self.parse_term()?;
        self.parse_sequence(
            |p| {
                let attribute_pos = p.current_position;
                let attribute = p.expect_keyword()?;
                match attribute.as_str() {
                    "named" => {
                        // If the term has a "named" attribute, we introduce a new nullary function
                        // definition that maps the name to the term
                        let name = p.expect_symbol()?;
                        p.state.function_defs.insert(
                            name,
                            FunctionDef {
                                params: Vec::new(),
                                body: inner.clone(),
                            },
                        );
                        Ok(())
                    }
                    "pattern" => {
                        // We just ignore the values of "pattern" attributes
                        p.expect_token(Token::OpenParen)?;
                        p.parse_sequence(Parser::parse_term, true)?;
                        Ok(())
                    }
                    _ => Err(Error::Parser(
                        ParserError::UnknownAttribute(attribute),
                        attribute_pos,
                    )),
                }
            },
            true,
        )?;
        Ok(inner)
    }

    fn parse_application(&mut self) -> AletheResult<Rc<Term>> {
        let head_pos = self.current_position;
        match &self.current_token {
            &Token::ReservedWord(reserved) => {
                self.next_token()?;
                match reserved {
                    Reserved::Exists => self.parse_quantifier(Quantifier::Exists),
                    Reserved::Forall => self.parse_quantifier(Quantifier::Forall),
                    Reserved::Choice => self.parse_choice_term(),
                    Reserved::Bang => self.parse_annotated_term(),
                    Reserved::Let => self.parse_let_term(),
                    _ => Err(Error::Parser(
                        ParserError::UnexpectedToken(Token::ReservedWord(reserved)),
                        head_pos,
                    )),
                }
            }
            // Here, I would like to use an `if let` guard, like:
            //
            //     Token::Symbol(s) if let Ok(operator) = Operator::from_str(s) => { ... }
            //
            // However, `if let` guards are still nightly only. For more info, see:
            // https://github.com/rust-lang/rust/issues/51114
            Token::Symbol(s) if Operator::from_str(s).is_ok() => {
                let operator = Operator::from_str(s).unwrap();
                self.next_token()?;
                let args = self.parse_sequence(Self::parse_term, true)?;
                self.make_op(operator, args)
                    .map_err(|err| Error::Parser(err, head_pos))
            }
            Token::Symbol(s) if self.state.function_defs.get(s).is_some() => {
                let head_pos = self.current_position;
                let func_name = self.expect_symbol()?;
                let args = self.parse_sequence(Self::parse_term, true)?;
                let func = self.state.function_defs.get(&func_name).unwrap();

                // If there is a function definition with this function name, we sort check
                // the arguments and apply the definition by performing a beta reduction.
                ParserError::assert_num_of_args(&args, func.params.len())
                    .map_err(|err| Error::Parser(err, head_pos))?;
                for (arg, param) in args.iter().zip(func.params.iter()) {
                    SortError::assert_eq(param.1.as_sort().unwrap(), arg.sort())
                        .map_err(|err| Error::Parser(err.into(), head_pos))?;
                }

                // Build a hash map of all the parameter names and the values they will
                // take
                let substitution = {
                    // We have to take a reference to the term pool here, so the closure in
                    // the `map` call later on doesn't have to capture all of `self`, and
                    // can just capture the term pool. We need this to please the borrow
                    // checker
                    let pool = &mut self.state.term_pool;
                    func.params
                        .iter()
                        .zip(args)
                        .map(|((name, sort), arg)| {
                            (pool.add_term(terminal!(var name; sort.clone())), arg)
                        })
                        .collect()
                };

                // Since we already checked the sorts of the arguments, creating and applying this
                // substitution can never fail
                let result = Substitution::new(&mut self.state.term_pool, substitution)
                    .unwrap()
                    .apply(&mut self.state.term_pool, &func.body)
                    .unwrap();

                Ok(result)
            }
            _ => {
                let func = self.parse_term()?;
                let args = self.parse_sequence(Self::parse_term, true)?;
                self.make_app(func, args)
                    .map_err(|err| Error::Parser(err, head_pos))
            }
        }
    }

    /// Parses a sort.
    fn parse_sort(&mut self) -> AletheResult<Term> {
        let pos = self.current_position;
        let (name, args) = match self.next_token()?.0 {
            Token::Symbol(s) => (s, Vec::new()),
            Token::OpenParen => {
                let name = self.expect_symbol()?;
                let args = self.parse_sequence(Parser::parse_sort, true)?;
                (name, self.add_all(args))
            }
            other => return Err(Error::Parser(ParserError::UnexpectedToken(other), pos)),
        };

        let sort = match name.as_str() {
            "Bool" | "Int" | "Real" | "String" if !args.is_empty() => Err(Error::Parser(
                ParserError::WrongNumberOfArgs(0, args.len()),
                pos,
            )),
            "Bool" => Ok(Sort::Bool),
            "Int" => Ok(Sort::Int),
            "Real" => Ok(Sort::Real),
            "String" => Ok(Sort::String),

            "Array" => match args.as_slice() {
                [x, y] => Ok(Sort::Array(x.clone(), y.clone())),
                _ => Err(Error::Parser(
                    ParserError::WrongNumberOfArgs(2, args.len()),
                    pos,
                )),
            },
            _ => match self.state.sort_declarations.get(&name) {
                Some(arity) if *arity == args.len() => Ok(Sort::Atom(name, args)),
                Some(arity) => Err(Error::Parser(
                    ParserError::WrongNumberOfArgs(*arity, args.len()),
                    pos,
                )),
                None => Err(Error::Parser(ParserError::UndefinedSort(name), pos)),
            },
        }?;
        Ok(Term::Sort(sort))
    }
}
