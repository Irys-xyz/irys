#!/bin/bash

# Help text
usage() {
    echo "Usage: $0 [-m max_iterations] 'command [args...]'"
    echo "  -m: Maximum number of iterations (default: infinite)"
    echo "Example: $0 -m 100 'RUST_BACKTRACE=1 cargo test --package irys-chain --test mod -- api::api::api_end_to_end_test_32b --exact --show-output'"
    exit 1
}

# Parse command line options
max_iterations=-1  # Default to infinite
while getopts "m:h" opt; do
    case $opt in
        m) max_iterations=$OPTARG ;;
        h) usage ;;
        ?) usage ;;
    esac
done

# Remove the options from the argument list
shift $((OPTIND > 1 ? OPTIND - 1 : 0))

# Check if a command was provided
if [ $# -eq 0 ]; then
    echo "Error: No command provided"
    usage
fi

# Reconstruct the full command from remaining arguments
command="$*"

# Create temporary file
LOGFILE=$(mktemp)
echo "Logging test output to: $LOGFILE"
echo "Command to run: $command"
if [ $max_iterations -ne -1 ]; then
    echo "Will run for max $max_iterations iterations"
fi

# Initialize counter
count=1

while true; do
    # Check if we've hit max iterations
    if [ $max_iterations -ne -1 ] && [ $count -gt $max_iterations ]; then
        echo "Reached maximum iterations ($max_iterations)"
        echo "All tests passed"
        exit 0
    fi

    echo "Test iteration: $count"
    echo "----------------------------------------"
    sleep 1s
    # Run the command and tee output to both terminal and log file
    eval "$command" 2>&1 | tee -a "$LOGFILE"

    # Check the exit status of the actual command, not tee
    if [ ${PIPESTATUS[0]} -ne 0 ]; then
        echo "----------------------------------------"
        echo "Test failed on iteration $count"
        echo "Full log file: $LOGFILE"
        echo "Last 20 lines of output:"
        echo "----------------------------------------"
        tail -n 20 "$LOGFILE"
        exit 1
    fi
    
    echo "Test passed"
    echo "----------------------------------------"
    
    # Increment counter
    ((count++))
done


# Clean up on exit
trap "rm -f $LOGFILE" EXIT



