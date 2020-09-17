use cosmwasm_std::{
    to_binary, Api, BankMsg, Binary, CanonicalAddr, Coin, CosmosMsg, Decimal, Env, Extern,
    HandleResponse, HumanAddr, InitResponse, Querier, QueryResult, StdError, StdResult, Storage,
    Uint128, WasmMsg,
};

use crate::msg::{
    space_pad, HandleAnswer, HandleMsg, InitMsg, QueryMsg,
    ResponseStatus::{Failure, Success},
};
use crate::state::{
    get_receiver_hash, get_transfers, read_allowance, read_viewing_key, set_receiver_hash,
    store_transfer, write_allowance, write_viewing_key, Balances, Config, Constants,
    ReadonlyBalances, ReadonlyConfig,
};
use crate::viewing_key::ViewingKey;

/// We make sure that responses from `handle` are padded to a multiple of this size.
const RESPONSE_BLOCK_SIZE: usize = 256;

pub fn init<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    _env: Env,
    msg: InitMsg,
) -> StdResult<InitResponse> {
    let mut total_supply: u128 = 0;
    {
        let mut balances = Balances::from_storage(&mut deps.storage);
        for balance in msg.initial_balances {
            let balance_address = deps.api.canonical_address(&balance.address)?;
            let amount = balance.amount.u128();
            balances.set_account_balance(&balance_address, amount);
            total_supply += amount;
        }
    }

    // Check name, symbol, decimals
    if !is_valid_name(&msg.name) {
        return Err(StdError::generic_err(
            "Name is not in the expected format (3-30 UTF-8 bytes)",
        ));
    }
    if !is_valid_symbol(&msg.symbol) {
        return Err(StdError::generic_err(
            "Ticker symbol is not in expected format [A-Z]{3,6}",
        ));
    }
    if msg.decimals > 18 {
        return Err(StdError::generic_err("Decimals must not exceed 18"));
    }

    let mut config = Config::from_storage(&mut deps.storage);
    config.set_constants(&Constants {
        name: msg.name,
        symbol: msg.symbol,
        decimals: msg.decimals,
    })?;
    config.set_total_supply(total_supply);

    Ok(InitResponse::default())
}

pub fn handle<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    msg: HandleMsg,
) -> StdResult<HandleResponse> {
    let response = match msg {
        // Native
        HandleMsg::Deposit { .. } => try_deposit(deps, env),
        HandleMsg::Withdraw /* todo rename Redeem */ { amount, .. } => try_withdraw(deps, env, amount),
        HandleMsg::Balance /* todo move to query? */ {..} => try_balance(deps, env),
        // Base
        HandleMsg::Transfer {
            recipient, amount, ..
        } => try_transfer(deps, env, &recipient, amount),
        HandleMsg::Send {
            recipient,
            amount,
            msg,
            ..
        } => try_send(deps, env, &recipient, amount, msg),
        HandleMsg::Burn { amount, .. } => try_burn(deps, env, amount),
        HandleMsg::RegisterReceive { code_hash, .. } => try_register_receive(deps, env, code_hash),
        HandleMsg::CreateViewingKey { entropy, .. } => try_create_key(deps, env, entropy),
        HandleMsg::SetViewingKey { key, .. } => try_set_key(deps, env, key),
        // Allowance
        // todo IncreaseAllowance
        // todo DecreaseAllowance
        HandleMsg::TransferFrom {
            owner,
            recipient,
            amount,
            ..
        } => try_transfer_from(deps, env, &owner, &recipient, amount),
        // todo SendFrom
        // todo BurnFrom
        HandleMsg::Allowance /* todo make query? */ { spender, .. } => try_check_allowance(deps, env, spender),
        HandleMsg::Approve /* todo unspecified??? */ {
            spender, amount, ..
        } => try_approve(deps, env, &spender, amount),
    };

    response.map(|mut response| {
        response.data = response.data.map(|mut data| {
            space_pad(RESPONSE_BLOCK_SIZE, &mut data.0);
            data
        });
        response
    })
}

pub fn query<S: Storage, A: Api, Q: Querier>(deps: &Extern<S, A, Q>, msg: QueryMsg) -> QueryResult {
    let (address, key) = msg.get_validation_params();

    let canonical_addr = deps.api.canonical_address(address)?;

    let expected_key = read_viewing_key(&deps.storage, &canonical_addr);

    // checking the key will take significant time. We don't want to exit immediately if it isn't set
    // in a way which will allow to time the command and determine if a viewing key doesn't exist
    if expected_key.is_none() && !key.check_viewing_key(&[0u8; 24]) {
        return Ok(Binary(
            b"Wrong viewing key for this address or viewing key not set".to_vec(),
        ));
    }

    if !key.check_viewing_key(expected_key.unwrap().as_slice()) {
        return Ok(Binary(
            b"Wrong viewing key for this address or viewing key not set".to_vec(),
        ));
    }

    match msg {
        // Base
        QueryMsg::Balance { address, .. } => query_balance(&deps, &address),
        // todo TokenInfo
        QueryMsg::Transfers /* todo rename TransferHistory */ { address, n, start, .. } => query_transactions(&deps, &address, start.unwrap_or(0), n),
        // Native
        // todo ExchangeRate
        // Other - Test
        _ => unimplemented!(),
    }
}

pub fn query_transactions<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    account: &HumanAddr,
    start: u32,
    count: u32,
) -> StdResult<Binary> {
    let address = deps.api.canonical_address(account).unwrap();
    let address = get_transfers(&deps.api, &deps.storage, &address, start, count)?;

    Ok(Binary(format!("{:?}", address).into_bytes().to_vec()))
}

pub fn query_balance<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    account: &HumanAddr,
) -> StdResult<Binary> {
    let address = deps.api.canonical_address(account)?;

    Ok(Binary(Vec::from(get_balance(deps, &address)?)))
}

pub fn try_set_key<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    key: String,
) -> StdResult<HandleResponse> {
    let vk = ViewingKey(key);

    if !vk.is_valid() {
        return Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: Some(to_binary(&HandleAnswer::SetViewingKey { status: Failure })?),
        });
    }

    let message_sender = deps.api.canonical_address(&env.message.sender)?;
    write_viewing_key(&mut deps.storage, &message_sender, &vk);

    Ok(HandleResponse {
        messages: vec![],
        log: vec![],
        data: Some(to_binary(&HandleAnswer::SetViewingKey { status: Success })?),
    })
}

pub fn try_create_key<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    entropy: String,
) -> StdResult<HandleResponse> {
    let vk = ViewingKey::new(&env, b"yo", (&entropy).as_ref());

    let message_sender = deps.api.canonical_address(&env.message.sender)?;
    write_viewing_key(&mut deps.storage, &message_sender, &vk);

    Ok(HandleResponse {
        messages: vec![],
        log: vec![],
        data: Some(to_binary(&HandleAnswer::CreateViewingKey {
            status: Success,
        })?),
    })
}

pub fn try_check_allowance<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    spender: HumanAddr,
) -> StdResult<HandleResponse> {
    let sender_address = deps.api.canonical_address(&env.message.sender)?;
    let allowance = read_allowance(
        &deps.storage,
        &sender_address,
        &deps.api.canonical_address(&spender)?,
    );

    if let Err(_e) = allowance {
        Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: None,
        })
    } else {
        Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: None,
        })
    }
}

pub fn try_balance<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
) -> StdResult<HandleResponse> {
    let sender_address = deps.api.canonical_address(&env.message.sender)?;
    let account_balance = get_balance(deps, &sender_address);

    if let Err(_e) = account_balance {
        Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: None,
        })
    } else {
        Ok(HandleResponse {
            messages: vec![],
            log: vec![],
            data: None,
        })
    }
}

fn get_balance<S: Storage, A: Api, Q: Querier>(
    deps: &Extern<S, A, Q>,
    account: &CanonicalAddr,
) -> StdResult<String> {
    let account_balance = ReadonlyBalances::from_storage(&deps.storage).account_amount(account);

    let consts = ReadonlyConfig::from_storage(&deps.storage).constants()?;

    Ok(to_display_token(
        account_balance,
        &consts.symbol,
        consts.decimals,
    ))
}

fn try_deposit<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
) -> StdResult<HandleResponse> {
    let mut amount = Uint128::zero();

    for coin in &env.message.sent_funds {
        if coin.denom == "uscrt" {
            amount = coin.amount
        }
    }

    if amount.is_zero() {
        return Err(StdError::generic_err("Lol send some funds dude"));
    }

    let amount = amount.u128();

    let sender_address = deps.api.canonical_address(&env.message.sender)?;

    let mut balances = Balances::from_storage(&mut deps.storage);
    let mut account_balance = balances.account_amount(&sender_address);
    account_balance += amount;
    balances.set_account_balance(&sender_address, account_balance);

    let mut config = Config::from_storage(&mut deps.storage);
    let mut total_supply = config.total_supply();
    total_supply += amount;
    config.set_total_supply(total_supply);

    let res = HandleResponse {
        messages: vec![],
        log: vec![],
        data: None,
    };

    Ok(res)
}

fn try_withdraw<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    amount: Uint128,
) -> StdResult<HandleResponse> {
    let sender_address = deps.api.canonical_address(&env.message.sender)?;
    let amount_raw = amount.u128();

    let mut balances = Balances::from_storage(&mut deps.storage);
    let mut account_balance = balances.account_amount(&sender_address);

    if account_balance < amount_raw {
        return Err(StdError::generic_err(format!(
            "insufficient funds to burn: balance={}, required={}",
            account_balance, amount_raw
        )));
    }
    account_balance -= amount_raw;

    balances.set_account_balance(&sender_address, account_balance);

    let mut config = Config::from_storage(&mut deps.storage);
    let mut total_supply = config.total_supply();
    total_supply -= amount_raw;
    config.set_total_supply(total_supply);

    let withdrawl_coins: Vec<Coin> = vec![Coin {
        denom: "uscrt".to_string(),
        amount,
    }];

    let res = HandleResponse {
        messages: vec![CosmosMsg::Bank(BankMsg::Send {
            from_address: env.contract.address,
            to_address: env.message.sender,
            amount: withdrawl_coins,
        })],
        log: vec![],
        data: None,
    };

    Ok(res)
}

fn try_transfer_impl<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    recipient: &HumanAddr,
    amount: Uint128,
) -> StdResult<()> {
    let sender_address = deps.api.canonical_address(&env.message.sender)?;
    let recipient_address = deps.api.canonical_address(recipient)?;

    perform_transfer(
        &mut deps.storage,
        &sender_address,
        &recipient_address,
        amount.u128(),
    )?;

    let symbol = Config::from_storage(&mut deps.storage).constants()?.symbol;

    store_transfer(
        &mut deps.storage,
        &sender_address,
        &recipient_address,
        amount,
        symbol,
    )?;

    Ok(())
}

fn try_transfer<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    recipient: &HumanAddr,
    amount: Uint128,
) -> StdResult<HandleResponse> {
    try_transfer_impl(deps, env, recipient, amount)?;
    let res = HandleResponse {
        messages: vec![],
        log: vec![],
        data: Some(to_binary(&HandleAnswer::Transfer { status: Success })?),
    };
    Ok(res)
}

fn try_send<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    recipient: &HumanAddr,
    amount: Uint128,
    msg: Binary,
) -> StdResult<HandleResponse> {
    try_transfer_impl(deps, env, recipient, amount)?;

    let receiver_hash = get_receiver_hash(&deps.storage, recipient);
    let mut messages = vec![];
    if let Some(receiver_hash) = receiver_hash {
        let receiver_hash = receiver_hash?;
        messages.push(CosmosMsg::Wasm(WasmMsg::Execute {
            msg,
            callback_code_hash: receiver_hash,
            contract_addr: recipient.clone(),
            send: vec![],
        }))
    }

    let res = HandleResponse {
        messages,
        log: vec![],
        data: Some(to_binary(&HandleAnswer::Send { status: Success })?),
    };
    Ok(res)
}

fn try_register_receive<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    code_hash: String,
) -> StdResult<HandleResponse> {
    set_receiver_hash(&mut deps.storage, &env.message.sender, code_hash);
    let res = HandleResponse {
        messages: vec![],
        log: vec![],
        data: Some(to_binary(&HandleAnswer::RegisterReceive {
            status: Success,
        })?),
    };
    Ok(res)
}

fn try_transfer_from<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    owner: &HumanAddr,
    recipient: &HumanAddr,
    amount: Uint128,
) -> StdResult<HandleResponse> {
    let spender_address = deps.api.canonical_address(&env.message.sender)?;
    let owner_address = deps.api.canonical_address(owner)?;
    let recipient_address = deps.api.canonical_address(recipient)?;
    let amount_raw = amount.u128();

    let mut allowance = read_allowance(&deps.storage, &owner_address, &spender_address)?;
    if allowance < amount_raw {
        return Err(StdError::generic_err(format!(
            "Insufficient allowance: allowance={}, required={}",
            allowance, amount_raw
        )));
    }
    allowance -= amount_raw;
    write_allowance(
        &mut deps.storage,
        &owner_address,
        &spender_address,
        allowance,
    )?;
    perform_transfer(
        &mut deps.storage,
        &owner_address,
        &recipient_address,
        amount_raw,
    )?;

    let symbol = Config::from_storage(&mut deps.storage).constants()?.symbol;

    store_transfer(
        &mut deps.storage,
        &owner_address,
        &recipient_address,
        amount,
        symbol,
    )?;

    let res = HandleResponse {
        messages: vec![],
        log: vec![],
        data: None,
    };
    Ok(res)
}

fn try_approve<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    spender: &HumanAddr,
    amount: Uint128,
) -> StdResult<HandleResponse> {
    let owner_address = deps.api.canonical_address(&env.message.sender)?;
    let spender_address = deps.api.canonical_address(spender)?;
    write_allowance(
        &mut deps.storage,
        &owner_address,
        &spender_address,
        amount.u128(),
    )?;
    let res = HandleResponse {
        messages: vec![],
        log: vec![],
        data: None,
    };
    Ok(res)
}

/// Burn tokens
///
/// Remove `amount` tokens from the system irreversibly, from signer account
///
/// @param amount the amount of money to burn
fn try_burn<S: Storage, A: Api, Q: Querier>(
    deps: &mut Extern<S, A, Q>,
    env: Env,
    amount: Uint128,
) -> StdResult<HandleResponse> {
    let sender_address = deps.api.canonical_address(&env.message.sender)?;
    let amount = amount.u128();

    let mut balances = Balances::from_storage(&mut deps.storage);
    let mut account_balance = balances.account_amount(&sender_address);

    if account_balance < amount {
        return Err(StdError::generic_err(format!(
            "insufficient funds to burn: balance={}, required={}",
            account_balance, amount
        )));
    }
    account_balance -= amount;

    balances.set_account_balance(&sender_address, account_balance);

    let mut config = Config::from_storage(&mut deps.storage);
    let mut total_supply = config.total_supply();
    total_supply -= amount;
    config.set_total_supply(total_supply);

    let res = HandleResponse {
        messages: vec![],
        log: vec![],
        data: Some(to_binary(&HandleAnswer::Burn { status: Success })?),
    };

    Ok(res)
}

fn perform_transfer<T: Storage>(
    store: &mut T,
    from: &CanonicalAddr,
    to: &CanonicalAddr,
    amount: u128,
) -> StdResult<()> {
    let mut balances = Balances::from_storage(store);

    let mut from_balance = balances.account_amount(from);
    if from_balance < amount {
        return Err(StdError::generic_err(format!(
            "Insufficient funds: balance={}, required={}",
            from_balance, amount
        )));
    }
    from_balance -= amount;
    balances.set_account_balance(from, from_balance);

    let mut to_balance = balances.account_amount(to);
    to_balance = to_balance.checked_add(amount).ok_or_else(|| {
        StdError::generic_err("This tx will literally make them too rich. Try transferring less")
    })?;
    balances.set_account_balance(to, to_balance);

    Ok(())
}

fn is_valid_name(name: &str) -> bool {
    let len = name.len();
    3 <= len && len <= 30
}

fn is_valid_symbol(symbol: &str) -> bool {
    let len = symbol.len();
    let len_is_valid = 3 <= len && len <= 6;

    len_is_valid && symbol.bytes().all(|byte| b'A' <= byte && byte <= b'Z')
}

fn to_display_token(amount: u128, symbol: &str, decimals: u8) -> String {
    let base: u32 = 10;

    let amnt: Decimal = Decimal::from_ratio(amount, (base.pow(decimals.into())) as u64);

    format!("{} {}", amnt, symbol)
}

// pub fn migrate<S: Storage, A: Api, Q: Querier>(
//     _deps: &mut Extern<S, A, Q>,
//     _env: Env,
//     _msg: MigrateMsg,
// ) -> StdResult<MigrateResponse> {
//     Ok(MigrateResponse::default())
// }
