use rhai::{Engine, EvalAltResult, ParseErrorType, INT};

#[test]
fn test_constant() -> Result<(), Box<EvalAltResult>> {
    let engine = Engine::new();

    assert_eq!(engine.eval::<INT>("const x = 123; x")?, 123);

    assert!(matches!(
        *engine.eval::<INT>("const x = 123; x = 42;").expect_err("expects error"),
        EvalAltResult::ErrorParsing(err) if err.error_type() == &ParseErrorType::AssignmentToConstant("x".to_string())
    ));

    #[cfg(not(feature = "no_index"))]
    assert!(matches!(
        *engine.eval::<INT>("const x = [1, 2, 3, 4, 5]; x[2] = 42;").expect_err("expects error"),
        EvalAltResult::ErrorParsing(err) if err.error_type() == &ParseErrorType::AssignmentToConstant("x".to_string())
    ));

    Ok(())
}
