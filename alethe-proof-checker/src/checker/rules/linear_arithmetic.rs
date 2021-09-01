use super::{to_option, RuleArgs};
use crate::ast::*;
use ahash::AHashMap;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};

pub fn la_rw_eq(RuleArgs { conclusion, .. }: RuleArgs) -> Option<()> {
    rassert!(conclusion.len() == 1);

    let ((t_1, u_1), ((t_2, u_2), (u_3, t_3))) = match_term!(
        (= (= t u) (and (<= t u) (<= u t))) = conclusion[0]
    )?;
    to_option(t_1 == t_2 && t_2 == t_3 && u_1 == u_2 && u_2 == u_3)
}

/// Converts a rational represented with division and negation to the resulting rational value. For
/// example, the term "(/ (- 5) 2)" is converted to the rational value "-2.5".
fn simple_operation_to_rational(term: &Term) -> Option<BigRational> {
    // TODO: Add tests for this
    if let Some((n, d)) = match_term!((/ n d) = term).or_else(|| match_term!((div n d) = term)) {
        Some(simple_operation_to_rational(n)? / simple_operation_to_rational(d)?)
    } else if let Some(t) = match_term!((-t) = term) {
        Some(-simple_operation_to_rational(t)?)
    } else {
        term.try_as_ratio()
    }
}

/// Takes a nested sequence of additions, subtractions and negations, and flattens it to a list of
/// terms and the polarity that they appear in. For example, the term "(+ (- x y) (+ (- z) w))" is
/// flattened to `[(x, true), (y, false), (z, false), (w, true)]`, where `true` representes
/// positive polarity.
fn flatten_sum(term: &Term) -> Vec<(&Term, bool)> {
    // TODO: Add tests for this
    // TODO: Add support for distributing numerical constant multiplications. For example, this
    // function should transform the term "(* 2 (+ (* 22 x) y 4))" into "(+ (* 44 x) (* 2 y) 8)".
    // Maybe it is more natural to merge this term with `LinearComb::from_term`.

    if let Some(args) = match_term!((+ ...) = term) {
        args.iter().flat_map(|t| flatten_sum(t.as_ref())).collect()
    } else if let Some(t) = match_term!((-t) = term) {
        let mut result = flatten_sum(t);
        result.iter_mut().for_each(|item| item.1 = !item.1);
        result
    } else if let Some(args) = match_term!((- ...) = term) {
        let mut result = flatten_sum(&args[0]);
        result.extend(args[1..].iter().flat_map(|t| {
            flatten_sum(t.as_ref())
                .into_iter()
                .map(|(t, polarity)| (t, !polarity))
        }));
        result
    } else {
        vec![(term, true)]
    }
}

/// Takes a disequality term and returns its negation, represented by an operator and arguments.
/// The disequality can be:
/// * An application of the "<", ">", "<=" or ">=" operators
/// * The negation of an application of any of these operator
/// * The negation of an application of the "=" operator
fn negate_disequality(term: &Term) -> Option<(Operator, &[ByRefRc<Term>])> {
    // TODO: Add tests for this
    use Operator::*;

    fn negate_operator(op: Operator) -> Option<Operator> {
        Some(match op {
            LessThan => GreaterEq,
            GreaterThan => LessEq,
            LessEq => GreaterThan,
            GreaterEq => LessThan,
            _ => return None,
        })
    }

    if let Some(Term::Op(op, args)) = match_term!((not t) = term) {
        if matches!(op, GreaterEq | LessEq | GreaterThan | LessThan | Equals) {
            return Some((*op, args));
        }
    } else if let Term::Op(op, args) = term {
        return Some((negate_operator(*op)?, args));
    }
    None
}

/// A linear combination, represented by a hash map from non-constant terms to their coefficients,
/// plus a constant term. This is also used to represent a disequality, in which case the left side
/// is the non-constant terms and their coefficients, and the right side is the constant term.
struct LinearComb<'a>(AHashMap<&'a Term, BigRational>, BigRational);

impl<'a> LinearComb<'a> {
    fn new() -> Self {
        Self(AHashMap::new(), BigRational::zero())
    }

    /// Builds a linear combination from a term. Only one constant term is allowed.
    fn from_term(term: &'a Term) -> Option<Self> {
        let mut result = Self(AHashMap::new(), BigRational::zero());
        for (arg, polarity) in flatten_sum(term) {
            let polarity_coeff = match polarity {
                true => BigRational::one(),
                false => -BigRational::one(),
            };
            match match_term!((* a b) = arg) {
                Some((a, b)) => {
                    let (var, coeff) = match (
                        simple_operation_to_rational(a),
                        simple_operation_to_rational(b),
                    ) {
                        (None, None) => (arg, BigRational::one()),
                        (None, Some(r)) => (a, r),
                        (Some(r), None) => (b, r),
                        (Some(_), Some(_)) => return None,
                    };
                    result.insert(var, coeff * polarity_coeff);
                }
                None => match simple_operation_to_rational(arg) {
                    Some(r) => result.1 += r * polarity_coeff,
                    None => result.insert(arg, polarity_coeff),
                },
            };
        }
        Some(result)
    }

    fn insert(&mut self, key: &'a Term, value: BigRational) {
        use std::collections::hash_map::Entry;

        match self.0.entry(key) {
            Entry::Occupied(mut e) => {
                let new_value = e.get() + value;
                if new_value.is_zero() {
                    e.remove();
                } else {
                    e.insert(new_value);
                }
            }
            Entry::Vacant(e) => {
                e.insert(value);
            }
        }
    }

    fn add(mut self, other: Self) -> Self {
        for (var, coeff) in other.0 {
            self.insert(var, coeff)
        }
        self.1 += other.1;
        self
    }

    fn mul(&mut self, scalar: &BigRational) {
        for coeff in self.0.values_mut() {
            *coeff *= scalar;
        }
        self.1 *= scalar;
    }

    fn neg(&mut self) {
        // We multiply by -1 instead of using the unary "-" operator because that would require
        // cloning. There is no simple way to negate in place
        self.mul(&-BigRational::one());
    }

    fn sub(self, mut other: Self) -> Self {
        other.neg();
        self.add(other)
    }
}

fn strengthen(op: Operator, disequality: &mut LinearComb, a: &BigRational) -> Operator {
    let is_integer = (&disequality.1 * a).is_integer();
    match op {
        Operator::GreaterEq if is_integer => op,

        // In some cases, when the disequality is over integers, we can make the strengthening
        // rules even stronger. Consider for instance the following example:
        //     (step t1 (cl
        //         (not (<= (- 1) n))
        //         (not (<= (- 1) (+ n m)))
        //         (<= (- 2) (* 2 n))
        //         (not (<= m 1))
        //     ) :rule la_generic :args (1 1 1 1))
        // After the third disequality is negated and flipped, it becomes:
        //     -2 * n > 2
        // If nothing fancy is done, this would strenghten to:
        //     -2 * n >= 3
        // However, in this case, we can divide the disequality by 2 before strengthening, and then
        // multiply it by 2 to get back. This would result in:
        //     -2 * n > 2
        //     -1 * n > 1
        //     -1 * n >= 2
        //     -2 * n >= 4
        // This is a stronger statement, and follows from the original disequality. To find the
        // value by which we should divide, we take the smallest coefficient in the left side of
        // the disequality.
        Operator::GreaterThan if is_integer => {
            let min = disequality
                .0
                .values()
                .map(|x| x.abs())
                .min()
                .unwrap_or_else(BigRational::one);

            // Instead of dividing and then multiplying back, we just multiply the "+ 1"
            // that is added by the strengthening rule
            disequality.1 = disequality.1.floor() + min;
            Operator::GreaterEq
        }
        Operator::GreaterThan | Operator::GreaterEq => {
            disequality.1 = disequality.1.floor() + BigRational::one();
            Operator::GreaterEq
        }
        Operator::LessThan | Operator::LessEq => unreachable!(),
        _ => op,
    }
}

pub fn la_generic(RuleArgs { conclusion, args, .. }: RuleArgs) -> Option<()> {
    rassert!(conclusion.len() == args.len());

    let final_disequality = conclusion
        .iter()
        .zip(args)
        .map(|(phi, a)| {
            let phi = phi.as_ref();
            let a = match a {
                ProofArg::Term(a) => simple_operation_to_rational(a)?,
                ProofArg::Assign(_, _) => return None,
            };

            // Steps 1 and 2: Negate the disequality
            let (mut op, args) = negate_disequality(phi)?;
            let (s1, s2) = match args {
                [s1, s2] => (LinearComb::from_term(s1)?, LinearComb::from_term(s2)?),
                _ => return None,
            };

            // Step 3: Move all non constant terms to the left side, and the d terms to the right.
            // We move everything to the left side by subtracting s2 from s1
            let mut disequality = s1.sub(s2);
            disequality.1 = -disequality.1; // We negate d to move it to the other side

            // If the operator is < or <=, we flip the disequality so it is > or >=
            if op == Operator::LessThan {
                disequality.neg();
                op = Operator::GreaterThan;
            } else if op == Operator::LessEq {
                disequality.neg();
                op = Operator::GreaterEq;
            }

            // Step 4: Apply strengthening rules
            let op = strengthen(op, &mut disequality, &a);

            // Step 5: Multiply disequality by a
            let a = match op {
                Operator::Equals => a,
                _ => a.abs(),
            };
            disequality.mul(&a);

            Some((op, disequality))
        })
        .try_fold(
            (Operator::Equals, LinearComb::new()),
            |(acc_op, acc), item| {
                let (op, diseq) = item?;
                let new_acc = acc.add(diseq);
                let new_op = match (acc_op, op) {
                    (_, Operator::GreaterEq) => Operator::GreaterEq,
                    (Operator::Equals, Operator::GreaterThan) => Operator::GreaterThan,
                    _ => acc_op,
                };
                Some((new_op, new_acc))
            },
        )?;

    let (op, LinearComb(left_side, right_side)) = final_disequality;

    // The left side must be empty, that is, equal to 0
    rassert!(left_side.is_empty());

    let is_disequality_true = {
        use std::cmp::Ordering;
        use Operator::*;

        // If the operator encompasses the actual relationship between 0 and the right side, the
        // disequality is true
        match BigRational::zero().cmp(&right_side) {
            Ordering::Less => matches!(op, LessThan | LessEq),
            Ordering::Equal => matches!(op, LessEq | GreaterEq | Equals),
            Ordering::Greater => matches!(op, GreaterThan | GreaterEq),
        }
    };

    // The final disequality must be contradictory
    to_option(!is_disequality_true)
}

pub fn la_disequality(RuleArgs { conclusion, .. }: RuleArgs) -> Option<()> {
    rassert!(conclusion.len() == 1);

    let ((t1_1, t2_1), (t1_2, t2_2), (t2_3, t1_3)) = match_term!(
        (or (= t1 t2) (not (<= t1 t2)) (not (<= t2 t1))) = conclusion[0]
    )?;
    to_option(t1_1 == t1_2 && t1_2 == t1_3 && t2_1 == t2_2 && t2_2 == t2_3)
}

#[cfg(test)]
mod tests {
    #[test]
    fn la_rw_eq() {
        test_cases! {
            definitions = "
                (declare-fun a () Int)
                (declare-fun b () Int)
                (declare-fun x () Real)
                (declare-fun y () Real)
            ",
            "Simple working examples" {
                "(step t1 (cl (= (= a b) (and (<= a b) (<= b a)))) :rule la_rw_eq)": true,
                "(step t1 (cl (= (= x y) (and (<= x y) (<= y x)))) :rule la_rw_eq)": true,
            }
            "Clause term is not of the correct form" {
                "(step t1 (cl (= (= b a) (and (<= a b) (<= b a)))) :rule la_rw_eq)": false,
                "(step t1 (cl (= (= x y) (and (<= x y) (<= x y)))) :rule la_rw_eq)": false,
            }
        }
    }

    #[test]
    fn la_generic() {
        test_cases! {
            definitions = "
                (declare-fun a () Real)
                (declare-fun b () Real)
                (declare-fun c () Real)
                (declare-fun m () Int)
                (declare-fun n () Int)
            ",
            "Simple working examples" {
                "(step t1 (cl (> a 0.0) (<= a 0.0)) :rule la_generic :args (1.0 1.0))": true,
                "(step t1 (cl (>= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 1.0))": true,
                "(step t1 (cl (<= 0.0 0.0)) :rule la_generic :args (1.0))": true,

                "(step t1 (cl (< (+ a b) 1.0) (> (+ a b) 0.0))
                    :rule la_generic :args (1.0 (- 1.0)))": true,

                "(step t1 (cl (<= (+ a (- b a)) b)) :rule la_generic :args (1.0))": true,

                "(step t1 (cl (not (<= (- a b) (- c 1.0))) (<= (+ 1.0 (- a c)) b))
                    :rule la_generic :args (1.0 1.0))": true,
            }
            "Empty clause" {
                "(step t1 (cl) :rule la_generic)": false,
            }
            "Wrong number of arguments" {
                "(step t1 (cl (>= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 1.0 1.0))": false,
            }
            "Invalid argument term" {
                "(step t1 (cl (>= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 b))": false,
            }
            "Clause term is not of the correct form" {
                "(step t1 (cl (ite (= a b) false true)) :rule la_generic :args (1.0))": false,
                "(step t1 (cl (= a 0.0) (< a 0.0)) :rule la_generic :args (1.0 1.0))": false,
            }
            "Negation of disequalities is satisfiable" {
                "(step t1 (cl (< 0.0 0.0)) :rule la_generic :args (1.0))": false,

                "(step t1 (cl (< (+ a b) 1.0) (> (+ a b c) 0.0))
                    :rule la_generic :args (1.0 (- 1.0)))": false,
            }
            "Edge case where the strengthening rules need to be stronger" {
                "(step t1 (cl
                    (not (<= (- 1) n))
                    (not (<= (- 1) (+ n m)))
                    (<= (- 2) (* 2 n))
                    (not (<= m 1))
                ) :rule la_generic :args (1 1 1 1))": true,
            }
        }
    }

    #[test]
    fn la_disequality() {
        test_cases! {
            definitions = "
                (declare-fun a () Int)
                (declare-fun b () Int)
                (declare-fun x () Real)
                (declare-fun y () Real)
            ",
            "Simple working examples" {
                "(step t1 (cl (or (= a b) (not (<= a b)) (not (<= b a))))
                    :rule la_disequality)": true,
                "(step t1 (cl (or (= x y) (not (<= x y)) (not (<= y x))))
                    :rule la_disequality)": true,
            }
            "Clause term is not of the correct form" {
                "(step t1 (cl (or (= b a) (not (<= a b)) (not (<= b a))))
                    :rule la_disequality)": false,
                "(step t1 (cl (or (= x y) (not (<= y x)) (not (<= y x))))
                    :rule la_disequality)": false,
            }
        }
    }
}