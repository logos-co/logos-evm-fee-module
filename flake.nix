{
  description = "Logos fee_module — EIP-1559 fee suggestion (slow/normal/fast) for EVM chains, derived from eth_feeHistory.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    eth_rpc_module.url = "github:logos-co/logos-evm-eth-rpc-module";
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
      targets = systems ++ [ "x86_64-windows" ];
      forAllTargets = f: nixpkgs.lib.genAttrs targets f;
    in
    {
      packages = forAllTargets (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
