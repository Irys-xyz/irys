module.exports =     // ecosystem.js
{
    "apps": [
        {
            "name": "Irys Node",
            "script": "./target/debug-release/irys",
            "exec": "none",
            "exec_mode": "fork",
            // "cron_restart": '*/45 * * * *',
            // "cron_restart": 0,
            "env": {
                "kill_timeout": 5_000,
                "RUST_LOG": "debug,jsonrpsee-server=off,irys::packing::progress=off,storage::db::mdbx=off",
                "RUST_BACKTRACE": "full"
            }
        }
    ]
};
