# Contracts
This is a small embedded `Foundry` project used to build smart contract artefacts used for testing.

To use this project, you have to:  
1.) clone the submodules present in `lib`, which should've happened when you initialized the project repository.
    if not, run `git submodule update --init --recursive --remote`  
2.) [install `foundry`](https://book.getfoundry.sh/getting-started/installation) to use the `forge` command. Once you have `foundry` installed:  
3.) build the contracts by running `forge build`   

If you want to create a new contract:   
1.) Add a new subfolder and then add a rule for the output folder in ./.gitignore in the contracts folder  
2.) Run `forge build`  
3.) Add the appropriate macros to your rust code, a good example is from crates/chain/tests/block_production/basic_contract.rs:   
```rs
// Codegen from artifact.
// taken from https://github.com/alloy-rs/examples/blob/main/examples/contracts/examples/deploy_from_artifact.rs
sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IrysERC20,
    "<../relative/path/to/>fixtures/contracts/out/IrysERC20.sol/IrysERC20.json"
);
```
  

### Foudry Documentation

https://book.getfoundry.sh/

### Usage

#### Build

```shell
$ forge build
```

#### Test

```shell
$ forge test
```

#### Format

```shell
$ forge fmt
```

#### Deploy

```shell
$ forge script script/Counter.s.sol:CounterScript --rpc-url <your_rpc_url> --private-key <your_private_key>
```

#### Help

```shell
$ forge --help
$ anvil --help
$ cast --help
```
