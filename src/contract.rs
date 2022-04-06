#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    coin, to_binary, Addr, BankMsg, Binary, Deps, DepsMut, DistributionMsg, Env,
    MessageInfo, QuerierWrapper, Response, StakingMsg, StdError, StdResult, Uint128, WasmMsg, WasmQuery, QueryRequest
};

use cw2::set_contract_version;
use cw20_base::allowances::{
    execute_burn_from, execute_decrease_allowance, execute_increase_allowance, execute_send_from,
    execute_transfer_from, query_allowance,
};
use cw20_base::contract::{
    execute_burn, execute_mint, execute_send, execute_transfer, query_balance, query_token_info,
};
use cw20_base::state::{MinterData, TokenInfo, TOKEN_INFO};

use crate::error::ContractError;
use crate::msg::{ExecuteMsg, InstantiateMsg, InvestmentResponse, QueryMsg};
use crate::state::{InvestmentInfo, Supply, CLAIMS, INVESTMENT, TOTAL_SUPPLY};

use cw20::{BalanceResponse, Cw20ExecuteMsg, Cw20QueryMsg};

// version info for migration info
const CONTRACT_NAME: &str = "crates.io:cw20-staking";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // ensure the validator is registered
    let vals = deps.querier.query_all_validators()?;
    if !vals.iter().any(|v| v.address == msg.validator) {
        return Err(ContractError::NotInValidatorSet {
            validator: msg.validator,
        });
    }

    // store token info using cw20-base format
    let data = TokenInfo {
        name: msg.name,
        symbol: msg.symbol,
        decimals: msg.decimals,
        total_supply: Uint128::zero(),
        // set self as minter, so we can properly execute mint and burn
        mint: Some(MinterData {
            minter: env.contract.address,
            cap: None,
        }),
    };
    TOKEN_INFO.save(deps.storage, &data)?;

    let denom = deps.querier.query_bonded_denom()?;
    let invest = InvestmentInfo {
        owner: info.sender,
        exit_tax: msg.exit_tax,
        unbonding_period: msg.unbonding_period,
        bond_denom: denom,
        validator: msg.validator,
        min_withdrawal: msg.min_withdrawal,
        staking_withdraw_address: msg.staking_withdraw_address
    };
    INVESTMENT.save(deps.storage, &invest)?;

    // set supply to 0
    let supply = Supply::default();
    TOTAL_SUPPLY.save(deps.storage, &supply)?;

    let res = Response::new()
    .add_message(DistributionMsg::SetWithdrawAddress {
        address: invest.staking_withdraw_address,
    });
    Ok(res)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::Bond {} => bond(deps, env, info),
        ExecuteMsg::Unbond { amount } => unbond(deps, env, info, amount),
        ExecuteMsg::Claim {} => claim(deps, env, info),
        ExecuteMsg::WithDrawDevCw20 { contract, amount } => withdraw_dev_cw20(deps, env, info, contract, amount),
        ExecuteMsg::SetStakingWithdrawAddress { addr } => set_staking_withdraw_address(deps, info, addr),
        ExecuteMsg::Redelegate { validator } => set_new_validator(deps, env, info, validator),

        // these all come from cw20-base to implement the cw20 standard
        ExecuteMsg::Transfer { recipient, amount } => {
            Ok(execute_transfer(deps, env, info, recipient, amount)?)
        }
        ExecuteMsg::Burn { amount } => Ok(execute_burn(deps, env, info, amount)?),
        ExecuteMsg::Send {
            contract,
            amount,
            msg,
        } => Ok(execute_send(deps, env, info, contract, amount, msg)?),
        ExecuteMsg::IncreaseAllowance {
            spender,
            amount,
            expires,
        } => Ok(execute_increase_allowance(
            deps, env, info, spender, amount, expires,
        )?),
        ExecuteMsg::DecreaseAllowance {
            spender,
            amount,
            expires,
        } => Ok(execute_decrease_allowance(
            deps, env, info, spender, amount, expires,
        )?),
        ExecuteMsg::TransferFrom {
            owner,
            recipient,
            amount,
        } => Ok(execute_transfer_from(
            deps, env, info, owner, recipient, amount,
        )?),
        ExecuteMsg::BurnFrom { owner, amount } => {
            Ok(execute_burn_from(deps, env, info, owner, amount)?)
        }
        ExecuteMsg::SendFrom {
            owner,
            contract,
            amount,
            msg,
        } => Ok(execute_send_from(
            deps, env, info, owner, contract, amount, msg,
        )?),
    }
}

// get_bonded returns the total amount of delegations from contract
// it ensures they are all the same denom
fn get_bonded(querier: &QuerierWrapper, contract: &Addr) -> Result<Uint128, ContractError> {
    let bonds = querier.query_all_delegations(contract)?;
    if bonds.is_empty() {
        return Ok(Uint128::zero());
    }
    let denom = bonds[0].amount.denom.as_str();
    bonds.iter().fold(Ok(Uint128::zero()), |racc, d| {
        let acc = racc?;
        if d.amount.denom.as_str() != denom {
            Err(ContractError::DifferentBondDenom {
                denom1: denom.into(),
                denom2: d.amount.denom.to_string(),
            })
        } else {
            Ok(acc + d.amount.amount)
        }
    })
}

fn assert_bonds(supply: &Supply, bonded: Uint128) -> Result<(), ContractError> {
    if supply.bonded != bonded {
        Err(ContractError::BondedMismatch {
            stored: supply.bonded,
            queried: bonded,
        })
    } else {
        Ok(())
    }
}

pub fn bond(deps: DepsMut, env: Env, info: MessageInfo) -> Result<Response, ContractError> {
    // ensure we have the proper denom
    let invest = INVESTMENT.load(deps.storage)?;
    // payment finds the proper coin (or throws an error)
    let payment = info
        .funds
        .iter()
        .find(|x| x.denom == invest.bond_denom)
        .ok_or_else(|| ContractError::EmptyBalance {
            denom: invest.bond_denom.clone(),
        })?;

    // bonded is the total number of tokens we have delegated from this address
    let bonded = get_bonded(&deps.querier, &env.contract.address)?;

    // calculate to_mint and update total supply
    let mut supply = TOTAL_SUPPLY.load(deps.storage)?;
    // TODO: this is just a safety assertion - do we keep it, or remove caching?
    // in the end supply is just there to cache the (expected) results of get_bonded() so we don't
    // have expensive queries everywhere
    assert_bonds(&supply, bonded)?;
    let to_mint = payment.amount;
    supply.bonded = bonded + payment.amount;
    TOTAL_SUPPLY.save(deps.storage, &supply)?;

    // call into cw20-base to mint the token, call as self as no one else is allowed
    let sub_info = MessageInfo {
        sender: env.contract.address.clone(),
        funds: vec![],
    };
    execute_mint(deps, env, sub_info, info.sender.to_string(), to_mint)?;

    // bond them to the validator
    let res = Response::new()
        .add_message(StakingMsg::Delegate {
            validator: invest.validator,
            amount: payment.clone(),
        })
        .add_attribute("action", "bond")
        .add_attribute("from", info.sender)
        .add_attribute("bonded", payment.amount)
        .add_attribute("minted", to_mint);
    Ok(res)
}

pub fn unbond(
    mut deps: DepsMut,
    env: Env,
    info: MessageInfo,
    amount: Uint128,
) -> Result<Response, ContractError> {
    let invest = INVESTMENT.load(deps.storage)?;
    // ensure it is big enough to care
    if amount < invest.min_withdrawal {
        return Err(ContractError::UnbondTooSmall {
            min_bonded: invest.min_withdrawal,
            denom: invest.bond_denom,
        });
    }
    // calculate tax and remainer to unbond
    let tax = amount * invest.exit_tax;

    // burn from the original caller
    execute_burn(deps.branch(), env.clone(), info.clone(), amount)?;
    if tax > Uint128::zero() {
        let sub_info = MessageInfo {
            sender: env.contract.address.clone(),
            funds: vec![],
        };
        // call into cw20-base to mint tokens to owner, call as self as no one else is allowed
        execute_mint(
            deps.branch(),
            env.clone(),
            sub_info,
            invest.owner.to_string(),
            tax,
        )?;
    }

    // re-calculate bonded to ensure we have real values
    // bonded is the total number of tokens we have delegated from this address
    let bonded = get_bonded(&deps.querier, &env.contract.address)?;

    // calculate how many native tokens this is worth and update supply
    let remainder = amount.checked_sub(tax).map_err(StdError::overflow)?;
    let mut supply = TOTAL_SUPPLY.load(deps.storage)?;
    // TODO: this is just a safety assertion - do we keep it, or remove caching?
    // in the end supply is just there to cache the (expected) results of get_bonded() so we don't
    // have expensive queries everywhere
    assert_bonds(&supply, bonded)?;
    let unbond = remainder;
    supply.bonded = bonded.checked_sub(unbond).map_err(StdError::overflow)?;
    supply.claims += unbond;
    TOTAL_SUPPLY.save(deps.storage, &supply)?;

    CLAIMS.create_claim(
        deps.storage,
        &info.sender,
        unbond,
        invest.unbonding_period.after(&env.block),
    )?;

    // unbond them
    let res = Response::new()
        .add_message(StakingMsg::Undelegate {
            validator: invest.validator,
            amount: coin(unbond.u128(), &invest.bond_denom),
        })
        .add_attribute("action", "unbond")
        .add_attribute("to", info.sender)
        .add_attribute("unbonded", unbond)
        .add_attribute("burnt", amount);
    Ok(res)
}

pub fn claim(deps: DepsMut, env: Env, info: MessageInfo) -> Result<Response, ContractError> {
    // find how many tokens the contract has
    let invest = INVESTMENT.load(deps.storage)?;
    let mut balance = deps
        .querier
        .query_balance(&env.contract.address, &invest.bond_denom)?;
    if balance.amount < invest.min_withdrawal {
        return Err(ContractError::BalanceTooSmall {});
    }

    // check how much to send - min(balance, claims[sender]), and reduce the claim
    // Ensure we have enough balance to cover this and only send some claims if that is all we can cover
    let to_send =
        CLAIMS.claim_tokens(deps.storage, &info.sender, &env.block, Some(balance.amount))?;
    if to_send == Uint128::zero() {
        return Err(ContractError::NothingToClaim {});
    }

    // update total supply (lower claim)
    TOTAL_SUPPLY.update(deps.storage, |mut supply| -> StdResult<_> {
        supply.claims = supply.claims.checked_sub(to_send)?;
        Ok(supply)
    })?;

    // transfer tokens to the sender
    balance.amount = to_send;
    let res = Response::new()
        .add_message(BankMsg::Send {
            to_address: info.sender.to_string(),
            amount: vec![balance],
        })
        .add_attribute("action", "claim")
        .add_attribute("from", info.sender)
        .add_attribute("amount", to_send);
    Ok(res)
}


pub fn set_new_validator(deps: DepsMut, env: Env, info: MessageInfo, validator: String) -> Result<Response, ContractError> {
    let mut invest = INVESTMENT.load(deps.storage)?;

    if invest.owner != info.sender {
        return Err(ContractError::Unauthorized {});
    }

    let vals = deps.querier.query_all_validators()?;
    if !vals.iter().any(|v| v.address == validator) {
        return Err(ContractError::NotInValidatorSet {
            validator: validator,
        });
    }

    let bonds = deps.querier.query_delegation(env.contract.address, invest.validator.as_str())?;
    let redelegate_coin = match bonds {
        Some(full_delegation) => {
            if full_delegation.can_redelegate.denom != invest.bond_denom {      
                Err(ContractError::DifferentBondDenom {
                    denom1: invest.bond_denom.clone(),
                    denom2: full_delegation.can_redelegate.denom.into(),
                })
            } else {
                Ok(full_delegation.can_redelegate)
            }
        },
        _ => Err(ContractError::EmptyBalance {
            denom: invest.bond_denom.clone(),
        }),
    }?;


    let res = Response::new()
        .add_message(StakingMsg::Redelegate {
            src_validator: invest.validator.clone(),
            dst_validator: validator.clone(),
            amount: redelegate_coin,
        });
    
    invest.validator = validator;
    INVESTMENT.save(deps.storage, &invest)?;
    Ok(res)
}

pub fn set_staking_withdraw_address(deps: DepsMut, info: MessageInfo, addr: String) -> Result<Response, ContractError> {
    
    let mut invest = INVESTMENT.load(deps.storage)?;

    if invest.owner != info.sender {
        return Err(ContractError::Unauthorized {});
    }

    invest.staking_withdraw_address = addr.clone();
    INVESTMENT.save(deps.storage, &invest)?;

    let res = Response::new()
        .add_message(DistributionMsg::SetWithdrawAddress {
            address: addr,
        });
    Ok(res)
}

pub fn withdraw_dev_cw20(deps: DepsMut, env: Env, info: MessageInfo, contract: String, amount: Uint128) -> Result<Response, ContractError> {
    
    let invest = INVESTMENT.load(deps.storage)?;

    if invest.owner != info.sender {
        return Err(ContractError::Unauthorized {});
    }
    let balance_res: BalanceResponse = deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
        contract_addr: contract.clone(),
        msg: to_binary(&Cw20QueryMsg::Balance {
            address: env.contract.address.into(),
        })?,
    }))?;

    let to_send = balance_res.balance.min(amount);

    let res = Response::new()
    .add_messages(vec![WasmMsg::Execute {
        contract_addr: contract.into(),
        msg: to_binary(&Cw20ExecuteMsg::Transfer {
            recipient: invest.owner.into(),
            amount: to_send,
        })?,
        funds: vec![],
    }]);
    Ok(res)
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        // custom queries
        QueryMsg::Claims { address } => {
            to_binary(&CLAIMS.query_claims(deps, &deps.api.addr_validate(&address)?)?)
        }
        QueryMsg::Investment {} => to_binary(&query_investment(deps)?),
        // inherited from cw20-base
        QueryMsg::TokenInfo {} => to_binary(&query_token_info(deps)?),
        QueryMsg::Balance { address } => to_binary(&query_balance(deps, address)?),
        QueryMsg::Allowance { owner, spender } => {
            to_binary(&query_allowance(deps, owner, spender)?)
        }
    }
}

pub fn query_investment(deps: Deps) -> StdResult<InvestmentResponse> {
    let invest = INVESTMENT.load(deps.storage)?;
    let supply = TOTAL_SUPPLY.load(deps.storage)?;

    let res = InvestmentResponse {
        owner: invest.owner.to_string(),
        exit_tax: invest.exit_tax,
        validator: invest.validator,
        min_withdrawal: invest.min_withdrawal,
        token_supply: supply.bonded,
        staked_tokens: coin(supply.bonded.u128(), &invest.bond_denom),
    };
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use cosmwasm_std::testing::{
        mock_dependencies, mock_env, mock_info, MockQuerier, MOCK_CONTRACT_ADDR,
    };
    use cosmwasm_std::{
        coins, Coin, CosmosMsg, Decimal, FullDelegation, OverflowError, OverflowOperation,
        Validator,
    };
    use cw_controllers::Claim;
    use cw0::{Duration, DAY, HOUR, WEEK};

    fn sample_validator(addr: &str) -> Validator {
        Validator {
            address: addr.into(),
            commission: Decimal::percent(3),
            max_commission: Decimal::percent(10),
            max_change_rate: Decimal::percent(1),
        }
    }

    fn sample_delegation(addr: &str, amount: Coin) -> FullDelegation {
        let can_redelegate = amount.clone();
        let accumulated_rewards = coins(0, &amount.denom);
        FullDelegation {
            validator: addr.into(),
            delegator: Addr::unchecked(MOCK_CONTRACT_ADDR),
            amount,
            can_redelegate,
            accumulated_rewards,
        }
    }

    fn set_validator(querier: &mut MockQuerier) {
        querier.update_staking("ustake", &[sample_validator(DEFAULT_VALIDATOR)], &[]);
    }

    fn set_delegation(querier: &mut MockQuerier, amount: u128, denom: &str) {
        querier.update_staking(
            "ustake",
            &[sample_validator(DEFAULT_VALIDATOR)],
            &[sample_delegation(DEFAULT_VALIDATOR, coin(amount, denom))],
        );
    }

    // just a test helper, forgive the panic
    fn later(env: &Env, delta: Duration) -> Env {
        let time_delta = match delta {
            Duration::Time(t) => t,
            _ => panic!("Must provide duration in time"),
        };
        let mut res = env.clone();
        res.block.time = res.block.time.plus_seconds(time_delta);
        res
    }

    const DEFAULT_VALIDATOR: &str = "default-validator";
    const DEFAULT_STAKING_WITHDRAW_ADDRESS: &str = "default-staking-withdraw-address";


    fn default_instantiate(tax_percent: u64, min_withdrawal: u128) -> InstantiateMsg {
        InstantiateMsg {
            name: "Cool Derivative".to_string(),
            symbol: "DRV".to_string(),
            decimals: 9,
            validator: String::from(DEFAULT_VALIDATOR),
            unbonding_period: DAY * 3,
            exit_tax: Decimal::percent(tax_percent),
            min_withdrawal: Uint128::new(min_withdrawal),
            staking_withdraw_address: String::from(DEFAULT_STAKING_WITHDRAW_ADDRESS),
        }
    }

    fn get_balance<U: Into<String>>(deps: Deps, addr: U) -> Uint128 {
        query_balance(deps, addr.into()).unwrap().balance
    }

    fn get_claims(deps: Deps, addr: &str) -> Vec<Claim> {
        CLAIMS
            .query_claims(deps, &Addr::unchecked(addr))
            .unwrap()
            .claims
    }

    #[test]
    fn instantiation_with_missing_validator() {
        let mut deps = mock_dependencies(&[]);
        deps.querier
            .update_staking("ustake", &[sample_validator("john")], &[]);

        let creator = String::from("creator");
        let msg = InstantiateMsg {
            name: "Cool Derivative".to_string(),
            symbol: "DRV".to_string(),
            decimals: 9,
            validator: String::from("my-validator"),
            unbonding_period: WEEK,
            exit_tax: Decimal::percent(2),
            min_withdrawal: Uint128::new(50),
            staking_withdraw_address: String::from("my-address"),
        };
        let info = mock_info(&creator, &[]);

        // make sure we can instantiate with this
        let err = instantiate(deps.as_mut(), mock_env(), info, msg).unwrap_err();
        assert_eq!(
            err,
            ContractError::NotInValidatorSet {
                validator: "my-validator".into()
            }
        );
    }

    #[test]
    fn proper_instantiation() {
        let mut deps = mock_dependencies(&[]);
        deps.querier.update_staking(
            "ustake",
            &[
                sample_validator("john"),
                sample_validator("mary"),
                sample_validator("my-validator"),
            ],
            &[],
        );

        let creator = String::from("creator");
        let msg = InstantiateMsg {
            name: "Cool Derivative".to_string(),
            symbol: "DRV".to_string(),
            decimals: 0,
            validator: String::from("my-validator"),
            unbonding_period: HOUR * 12,
            exit_tax: Decimal::percent(2),
            min_withdrawal: Uint128::new(50),
            staking_withdraw_address: String::from("my-address"),
        };
        let info = mock_info(&creator, &[]);

        // make sure we can instantiate with this
        let res = instantiate(deps.as_mut(), mock_env(), info, msg.clone()).unwrap();
        assert_eq!(0, res.messages.len());

        // token info is proper
        let token = query_token_info(deps.as_ref()).unwrap();
        assert_eq!(&token.name, &msg.name);
        assert_eq!(&token.symbol, &msg.symbol);
        assert_eq!(token.decimals, msg.decimals);
        assert_eq!(token.total_supply, Uint128::zero());

        // no balance
        assert_eq!(get_balance(deps.as_ref(), &creator), Uint128::zero());
        // no claims
        assert_eq!(get_claims(deps.as_ref(), &creator), vec![]);

        // investment info correct
        let invest = query_investment(deps.as_ref()).unwrap();
        assert_eq!(&invest.owner, &creator);
        assert_eq!(&invest.validator, &msg.validator);
        assert_eq!(invest.exit_tax, msg.exit_tax);
        assert_eq!(invest.min_withdrawal, msg.min_withdrawal);

        assert_eq!(invest.token_supply, Uint128::zero());
        assert_eq!(invest.staked_tokens, coin(0, "ustake"));
    }

    #[test]
    fn bonding_issues_tokens() {
        let mut deps = mock_dependencies();
        set_validator(&mut deps.querier);

        let creator = String::from("creator");
        let instantiate_msg = default_instantiate(2, 50);
        let info = mock_info(&creator, &[]);

        // make sure we can instantiate with this
        let res = instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();
        assert_eq!(0, res.messages.len());

        // let's bond some tokens now
        let bob = String::from("bob");
        let bond_msg = ExecuteMsg::Bond {};
        let info = mock_info(&bob, &[coin(10, "random"), coin(1000, "ustake")]);

        // try to bond and make sure we trigger delegation
        let res = execute(deps.as_mut(), mock_env(), info, bond_msg).unwrap();
        assert_eq!(1, res.messages.len());
        let delegate = &res.messages[0];
        match &delegate.msg {
            CosmosMsg::Staking(StakingMsg::Delegate { validator, amount }) => {
                assert_eq!(validator.as_str(), DEFAULT_VALIDATOR);
                assert_eq!(amount, &coin(1000, "ustake"));
            }
            _ => panic!("Unexpected message: {:?}", delegate),
        }

        // bob got 1000 DRV for 1000 stake at a 1.0 ratio
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(1000));

        // investment info correct (updated supply)
        let invest = query_investment(deps.as_ref()).unwrap();
        assert_eq!(invest.token_supply, Uint128::new(1000));
        assert_eq!(invest.staked_tokens, coin(1000, "ustake"));

        // token info also properly updated
        let token = query_token_info(deps.as_ref()).unwrap();
        assert_eq!(token.total_supply, Uint128::new(1000));
    }

    #[test]
    fn rebonding_changes_pricing() {
        let mut deps = mock_dependencies();
        set_validator(&mut deps.querier);

        let creator = String::from("creator");
        let instantiate_msg = default_instantiate(2, 50);
        let info = mock_info(&creator, &[]);

        // make sure we can instantiate with this
        let res = instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();
        assert_eq!(0, res.messages.len());

        // let's bond some tokens now
        let bob = String::from("bob");
        let bond_msg = ExecuteMsg::Bond {};
        let info = mock_info(&bob, &[coin(10, "random"), coin(1000, "ustake")]);
        let res = execute(deps.as_mut(), mock_env(), info, bond_msg).unwrap();
        assert_eq!(1, res.messages.len());

        // update the querier with new bond
        set_delegation(&mut deps.querier, 1000, "ustake");

        // fake a reinvestment (this must be sent by the contract itself)
        let rebond_msg = ExecuteMsg::_BondAllTokens {};
        let info = mock_info(MOCK_CONTRACT_ADDR, &[]);
        deps.querier
            .update_balance(MOCK_CONTRACT_ADDR, coins(500, "ustake"));
        let _ = execute(deps.as_mut(), mock_env(), info, rebond_msg).unwrap();

        // update the querier with new bond
        set_delegation(&mut deps.querier, 1500, "ustake");

        // we should now see 1000 issues and 1500 bonded (and a price of 1.5)
        let invest = query_investment(deps.as_ref()).unwrap();
        assert_eq!(invest.token_supply, Uint128::new(1000));
        assert_eq!(invest.staked_tokens, coin(1500, "ustake"));

        // we bond some other tokens and get a different issuance price (maintaining the ratio)
        let alice = String::from("alice");
        let bond_msg = ExecuteMsg::Bond {};
        let info = mock_info(&alice, &[coin(3000, "ustake")]);
        let res = execute(deps.as_mut(), mock_env(), info, bond_msg).unwrap();
        assert_eq!(1, res.messages.len());

        // update the querier with new bond
        set_delegation(&mut deps.querier, 3000, "ustake");

        // alice should have gotten 2000 DRV for the 3000 stake, keeping the ratio at 1.5
        assert_eq!(get_balance(deps.as_ref(), &alice), Uint128::new(2000));

        let invest = query_investment(deps.as_ref()).unwrap();
        assert_eq!(invest.token_supply, Uint128::new(3000));
        assert_eq!(invest.staked_tokens, coin(4500, "ustake"));
    }

    #[test]
    fn bonding_fails_with_wrong_denom() {
        let mut deps = mock_dependencies();
        set_validator(&mut deps.querier);

        let creator = String::from("creator");
        let instantiate_msg = default_instantiate(2, 50);
        let info = mock_info(&creator, &[]);

        // make sure we can instantiate with this
        let res = instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();
        assert_eq!(0, res.messages.len());

        // let's bond some tokens now
        let bob = String::from("bob");
        let bond_msg = ExecuteMsg::Bond {};
        let info = mock_info(&bob, &[coin(500, "photon")]);

        // try to bond and make sure we trigger delegation
        let err = execute(deps.as_mut(), mock_env(), info, bond_msg).unwrap_err();
        assert_eq!(
            err,
            ContractError::EmptyBalance {
                denom: "ustake".to_string()
            }
        );
    }

    #[test]
    fn unbonding_maintains_price_ratio() {
        let mut deps = mock_dependencies();
        set_validator(&mut deps.querier);

        let creator = String::from("creator");
        let instantiate_msg = default_instantiate(10, 50);
        let info = mock_info(&creator, &[]);

        // make sure we can instantiate with this
        let res = instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();
        assert_eq!(0, res.messages.len());

        // let's bond some tokens now
        let bob = String::from("bob");
        let bond_msg = ExecuteMsg::Bond {};
        let info = mock_info(&bob, &[coin(10, "random"), coin(1000, "ustake")]);
        let res = execute(deps.as_mut(), mock_env(), info, bond_msg).unwrap();
        assert_eq!(1, res.messages.len());

        // update the querier with new bond
        set_delegation(&mut deps.querier, 1000, "ustake");

        // fake a reinvestment (this must be sent by the contract itself)
        // after this, we see 1000 issues and 1500 bonded (and a price of 1.5)
        let rebond_msg = ExecuteMsg::_BondAllTokens {};
        let info = mock_info(MOCK_CONTRACT_ADDR, &[]);
        deps.querier
            .update_balance(MOCK_CONTRACT_ADDR, coins(500, "ustake"));
        let _ = execute(deps.as_mut(), mock_env(), info, rebond_msg).unwrap();

        // update the querier with new bond, lower balance
        set_delegation(&mut deps.querier, 1500, "ustake");
        deps.querier.update_balance(MOCK_CONTRACT_ADDR, vec![]);

        // creator now tries to unbond these tokens - this must fail
        let unbond_msg = ExecuteMsg::Unbond {
            amount: Uint128::new(600),
        };
        let info = mock_info(&creator, &[]);
        let err = execute(deps.as_mut(), mock_env(), info, unbond_msg).unwrap_err();

        // bob unbonds 600 tokens at 10% tax...
        // 60 are taken and send to the owner
        // 540 are unbonded in exchange for 540 * 1.5 = 810 native tokens
        let unbond_msg = ExecuteMsg::Unbond {
            amount: Uint128::new(600),
        };
        let owner_cut = Uint128::new(60);
        let bobs_claim = Uint128::new(810);
        let bobs_balance = Uint128::new(400);
        let env = mock_env();
        let info = mock_info(&bob, &[]);
        let res = execute(deps.as_mut(), env.clone(), info, unbond_msg).unwrap();
        assert_eq!(1, res.messages.len());
        let delegate = &res.messages[0];
        match &delegate.msg {
            CosmosMsg::Staking(StakingMsg::Undelegate { validator, amount }) => {
                assert_eq!(validator.as_str(), DEFAULT_VALIDATOR);
                assert_eq!(amount, &coin(bobs_claim.u128(), "ustake"));
            }
            _ => panic!("Unexpected message: {:?}", delegate),
        }

        // update the querier with new bond, lower balance
        set_delegation(&mut deps.querier, 690, "ustake");

        // check balances
        assert_eq!(get_balance(deps.as_ref(), &bob), bobs_balance);
        assert_eq!(get_balance(deps.as_ref(), &creator), owner_cut);
        // proper claims
        let expected_claims = vec![Claim {
            amount: bobs_claim,
            release_at: (DAY * 3).after(&env.block),
        }];
        assert_eq!(expected_claims, get_claims(deps.as_ref(), &bob));

        // supplies updated, ratio the same (1.5)
        let ratio = Decimal::from_str("1.5").unwrap();

        let invest = query_investment(deps.as_ref()).unwrap();
        assert_eq!(invest.token_supply, bobs_balance + owner_cut);
        assert_eq!(invest.staked_tokens, coin(690, "ustake")); // 1500 - 810
    }

    #[test]
    fn claims_paid_out_properly() {
        let mut deps = mock_dependencies();
        set_validator(&mut deps.querier);

        // create contract
        let creator = String::from("creator");
        let instantiate_msg = default_instantiate(10, 50);
        let info = mock_info(&creator, &[]);
        instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();

        // bond some tokens
        let bob = String::from("bob");
        let info = mock_info(&bob, &coins(1000, "ustake"));
        execute(deps.as_mut(), mock_env(), info, ExecuteMsg::Bond {}).unwrap();
        set_delegation(&mut deps.querier, 1000, "ustake");

        // unbond part of them
        let unbond_msg = ExecuteMsg::Unbond {
            amount: Uint128::new(600),
        };
        let env = mock_env();
        let info = mock_info(&bob, &[]);
        execute(deps.as_mut(), env.clone(), info.clone(), unbond_msg).unwrap();
        set_delegation(&mut deps.querier, 460, "ustake");

        // ensure claims are proper
        let bobs_claim = Uint128::new(540);
        let original_claims = vec![Claim {
            amount: bobs_claim,
            release_at: (DAY * 3).after(&env.block),
        }];
        assert_eq!(original_claims, get_claims(deps.as_ref(), &bob));

        // bob cannot exercise claims without enough balance
        let claim_ready = later(&env, (DAY * 3 + HOUR).unwrap());
        let too_soon = later(&env, DAY);
        let fail = execute(
            deps.as_mut(),
            claim_ready.clone(),
            info.clone(),
            ExecuteMsg::Claim {},
        );
        assert!(fail.is_err(), "{:?}", fail);

        // provide the balance, but claim not yet mature - also prohibited
        deps.querier
            .update_balance(MOCK_CONTRACT_ADDR, coins(540, "ustake"));
        let fail = execute(deps.as_mut(), too_soon, info.clone(), ExecuteMsg::Claim {});
        assert!(fail.is_err(), "{:?}", fail);

        // this should work with cash and claims ready
        let res = execute(deps.as_mut(), claim_ready, info, ExecuteMsg::Claim {}).unwrap();
        assert_eq!(1, res.messages.len());
        let payout = &res.messages[0];
        match &payout.msg {
            CosmosMsg::Bank(BankMsg::Send { to_address, amount }) => {
                assert_eq!(amount, &coins(540, "ustake"));
                assert_eq!(to_address, &bob);
            }
            _ => panic!("Unexpected message: {:?}", payout),
        }

        // claims have been removed
        assert_eq!(get_claims(deps.as_ref(), &bob), vec![]);
    }

    #[test]
    fn cw20_imports_work() {
        let mut deps = mock_dependencies();
        set_validator(&mut deps.querier);

        // set the actors... bob stakes, sends coins to carl, and gives allowance to alice
        let bob = String::from("bob");
        let alice = String::from("alice");
        let carl = String::from("carl");

        // create the contract
        let creator = String::from("creator");
        let instantiate_msg = default_instantiate(2, 50);
        let info = mock_info(&creator, &[]);
        instantiate(deps.as_mut(), mock_env(), info, instantiate_msg).unwrap();

        // bond some tokens to create a balance
        let info = mock_info(&bob, &[coin(10, "random"), coin(1000, "ustake")]);
        execute(deps.as_mut(), mock_env(), info, ExecuteMsg::Bond {}).unwrap();

        // bob got 1000 DRV for 1000 stake at a 1.0 ratio
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(1000));

        // send coins to carl
        let bob_info = mock_info(&bob, &[]);
        let transfer = ExecuteMsg::Transfer {
            recipient: carl.clone(),
            amount: Uint128::new(200),
        };
        execute(deps.as_mut(), mock_env(), bob_info.clone(), transfer).unwrap();
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(800));
        assert_eq!(get_balance(deps.as_ref(), &carl), Uint128::new(200));

        // allow alice
        let allow = ExecuteMsg::IncreaseAllowance {
            spender: alice.clone(),
            amount: Uint128::new(350),
            expires: None,
        };
        execute(deps.as_mut(), mock_env(), bob_info.clone(), allow).unwrap();
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(800));
        assert_eq!(get_balance(deps.as_ref(), &alice), Uint128::zero());
        assert_eq!(
            query_allowance(deps.as_ref(), bob.clone(), alice.clone())
                .unwrap()
                .allowance,
            Uint128::new(350)
        );

        // alice takes some for herself
        let self_pay = ExecuteMsg::TransferFrom {
            owner: bob.clone(),
            recipient: alice.clone(),
            amount: Uint128::new(250),
        };
        let alice_info = mock_info(&alice, &[]);
        execute(deps.as_mut(), mock_env(), alice_info, self_pay).unwrap();
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(550));
        assert_eq!(get_balance(deps.as_ref(), &alice), Uint128::new(250));
        assert_eq!(
            query_allowance(deps.as_ref(), bob.clone(), alice)
                .unwrap()
                .allowance,
            Uint128::new(100)
        );

        // burn some, but not too much
        let burn_too_much = ExecuteMsg::Burn {
            amount: Uint128::new(1000),
        };
        let failed = execute(deps.as_mut(), mock_env(), bob_info.clone(), burn_too_much);
        assert!(failed.is_err());
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(550));
        let burn = ExecuteMsg::Burn {
            amount: Uint128::new(130),
        };
        execute(deps.as_mut(), mock_env(), bob_info, burn).unwrap();
        assert_eq!(get_balance(deps.as_ref(), &bob), Uint128::new(420));
    }
}
