// Copyright 2020 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use cfx_parameters::internal_contract_addresses::ADMIN_CONTROL_CONTRACT_ADDRESS;

use super::{
    super::impls::admin::*, ExecutionTrait, InterfaceTrait,
    InternalContractTrait, PreExecCheckConfTrait, SolFnTable,
    SolidityFunctionTrait, UpfrontPaymentTrait,
};
#[cfg(test)]
use crate::check_signature;
use crate::{
    evm::{ActionParams, Spec},
    impl_function_type, make_function_table, make_solidity_contract,
    make_solidity_function,
    trace::{trace::ExecTrace, Tracer},
    vm::{self, Env},
};
use cfx_state::{state_trait::StateOpsTrait, SubstateTrait};
use cfx_types::{Address, U256};
#[cfg(test)]
use rustc_hex::FromHex;

fn generate_fn_table() -> SolFnTable {
    make_function_table!(SetAdmin, Destroy, GetAdmin)
}

make_solidity_contract! {
    pub struct AdminControl(ADMIN_CONTROL_CONTRACT_ADDRESS, generate_fn_table);
}

make_solidity_function! {
    struct SetAdmin((Address, Address), "setAdmin(address,address)");
}
impl_function_type!(SetAdmin, "non_payable_write", gas: |spec: &Spec| spec.sstore_reset_gas);

impl ExecutionTrait for SetAdmin {
    fn execute_inner(
        &self, inputs: (Address, Address), params: &ActionParams, _env: &Env,
        _spec: &Spec, state: &mut dyn StateOpsTrait,
        substate: &mut dyn SubstateTrait,
        _tracer: &mut dyn Tracer<Output = ExecTrace>,
    ) -> vm::Result<()>
    {
        set_admin(
            inputs.0,
            inputs.1,
            substate.contract_in_creation(),
            params,
            state,
        )
    }
}

make_solidity_function! {
    struct Destroy(Address, "destroy(address)");
}
impl_function_type!(Destroy, "non_payable_write", gas: |spec: &Spec| spec.sstore_reset_gas);

impl ExecutionTrait for Destroy {
    fn execute_inner(
        &self, input: Address, params: &ActionParams, env: &Env, spec: &Spec,
        state: &mut dyn StateOpsTrait, substate: &mut dyn SubstateTrait,
        tracer: &mut dyn Tracer<Output = ExecTrace>,
    ) -> vm::Result<()>
    {
        destroy(input, params, state, spec, substate, tracer, env)
    }
}

make_solidity_function! {
    struct GetAdmin(Address, "getAdmin(address)", Address);
}
impl_function_type!(GetAdmin, "query_with_default_gas");

impl ExecutionTrait for GetAdmin {
    fn execute_inner(
        &self, input: Address, _: &ActionParams, _env: &Env, _: &Spec,
        state: &mut dyn StateOpsTrait, _: &mut dyn SubstateTrait,
        _tracer: &mut dyn Tracer<Output = ExecTrace>,
    ) -> vm::Result<Address>
    {
        Ok(state.admin(&input)?)
    }
}

#[test]
fn test_admin_contract_sig_v2() {
    // Check the consistency between signature generated by rust code and java
    // sdk.
    check_signature!(GetAdmin, "64efb22b");
    check_signature!(SetAdmin, "c55b6bb7");
    check_signature!(Destroy, "00f55d9d");
}
