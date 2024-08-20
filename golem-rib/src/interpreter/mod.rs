pub use env::RibFunctionInvoke;
pub use literal::*;
pub use result::*;
pub use rib_interpreter::*;

use crate::RibByteCode;
use golem_wasm_rpc::protobuf::type_annotated_value::TypeAnnotatedValue;
use std::collections::HashMap;

mod env;
mod literal;
mod result;
mod rib_interpreter;
mod stack;

pub async fn run_byte_code(
    rib: &RibByteCode,
    rib_input: RibInput,
    function_invoke: RibFunctionInvoke,
) -> Result<RibInterpreterResult, String> {
    let mut interpreter = Interpreter::new(rib_input.input, function_invoke);
    interpreter.run(rib.clone()).await
}

#[derive(Clone, Debug)]
pub struct RibInput {
    pub input: HashMap<String, TypeAnnotatedValue>,
}

impl RibInput {
    pub fn empty() -> Self {
        RibInput {
            input: HashMap::new(),
        }
    }

    pub fn new(input: HashMap<String, TypeAnnotatedValue>) -> Self {
        RibInput { input }
    }

    pub fn insert(&mut self, key: String, value: TypeAnnotatedValue) {
        self.input.insert(key, value);
    }

    pub fn merge(&mut self, other: RibInput) {
        self.input.extend(other.input);
    }

    pub fn lookup(&self, key: &str) -> Option<&TypeAnnotatedValue> {
        self.input.get(key)
    }
}