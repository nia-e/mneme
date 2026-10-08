/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::data::expr::Op;
use crate::data::functions::OP_ABS;
use crate::{DataValue, DbInstance};

#[test]
fn serialized_op_requires_the_wire_prefix() {
    let invalid = rmp_serde::to_vec("abs").unwrap();
    let result: Result<&'static Op, _> = rmp_serde::from_slice(&invalid);
    let error = match result {
        Ok(_) => panic!("deserialized an op without its OP_ wire prefix"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("invalid serialized op name"));

    let valid = rmp_serde::to_vec("OP_ABS").unwrap();
    let decoded: &'static Op = rmp_serde::from_slice(&valid).unwrap();
    assert_eq!(decoded, &OP_ABS);
}

#[test]
fn expression_eval() {
    let db = DbInstance::default();

    let res = db
        .run_default(
            r#"
    ?[a] := a = if(2 + 3 > 1 * 99999, 190291021 + 14341234212 / 2121)
    "#,
        )
        .unwrap();
    assert_eq!(res.rows[0][0], DataValue::Null);

    let res = db
        .run_default(
            r#"
    ?[a] := a = if(2 + 3 > 1, true, false)
    "#,
        )
        .unwrap();
    assert!(res.rows[0][0].get_bool().unwrap());
}
