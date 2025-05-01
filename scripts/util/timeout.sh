#!/bin/bash

usage() {
    echo "Usage: $0 -c COMMAND -n ITERATIONS -t TIMEOUT_SECONDS [-v] [-e]"
    echo "  -c COMMAND          Command to execute (use quotes for commands with arguments)"
    echo "  -n ITERATIONS       Number of times to run the command"
    echo "  -t TIMEOUT_SECONDS  Timeout in seconds for each command execution"
    echo "  -v                  Verbose mode (optional)"
    echo "  -e                  Exit if command completes successfully (without timing out)"
    exit 1
}

# Defaults
ITERATIONS=1
TIMEOUT=30
VERBOSE=false
EXIT_ON_SUCCESS=false

while getopts "c:n:t:veh" opt; do
    case ${opt} in
        c) COMMAND="$OPTARG" ;;
        n) ITERATIONS=$OPTARG ;;
        t) TIMEOUT=$OPTARG ;;
        v) VERBOSE=true ;;
        e) EXIT_ON_SUCCESS=true ;;
        h) usage ;;
        *) usage ;;
    esac
done

if [ -z "$COMMAND" ]; then
    echo "Error: Command is required"
    usage
fi

if ! [[ "$ITERATIONS" =~ ^[0-9]+$ ]]; then
    echo "Error: Iterations must be a positive integer"
    exit 1
fi

if ! [[ "$TIMEOUT" =~ ^[0-9]+$ ]]; then
    echo "Error: Timeout must be a positive integer"
    exit 1
fi

if [ "$VERBOSE" = true ]; then
    echo "Command: $COMMAND"
    echo "Iterations: $ITERATIONS"
    echo "Timeout: $TIMEOUT seconds"
    echo "Exit on success: $([ "$EXIT_ON_SUCCESS" = true ] && echo "enabled" || echo "disabled")"
    echo "Starting execution..."
fi

for (( i=1; i<=ITERATIONS; i++ )); do
    if [ "$VERBOSE" = true ]; then
        echo -e "\nIteration $i of $ITERATIONS"
        echo "========================================"
    fi

    timeout_pid=""
    exit_code=""
    
    # Start the command in background
    eval "$COMMAND" &
    cmd_pid=$!
    
    if [ "$VERBOSE" = true ]; then
        echo "Running command (PID: $cmd_pid) with $TIMEOUT second timeout..."
    fi
    
    # Wait for command to complete or timeout to expire
    elapsed=0
    while [ $elapsed -lt $TIMEOUT ]; do
        if ! kill -0 $cmd_pid 2>/dev/null; then
            # Command has finished
            wait $cmd_pid
            exit_code=$?
            if [ "$VERBOSE" = true ]; then
                echo "Command exited before timeout with exit code: $exit_code"
            else
                echo "Command exited with code: $exit_code (after $elapsed seconds)"
            fi
            break
        fi
        sleep 1
        ((elapsed++))
    done
    
    # Check if command is still running (timeout occurred)
    if kill -0 $cmd_pid 2>/dev/null; then
        if [ "$VERBOSE" = true ]; then
            echo "Command timed out after $TIMEOUT seconds. Force killing..."
        fi
        
        # get all child processes
        child_pids=$(pgrep -P $cmd_pid)
        
        # force kill the children first
        if [ ! -z "$child_pids" ]; then
            if [ "$VERBOSE" = true ]; then
                echo "Killing child processes: $child_pids"
            fi
            kill -9 $child_pids 2>/dev/null
        fi
        
        # then kill the main process
        kill -9 $cmd_pid 2>/dev/null
        
        # wait for the process to fully terminate
        while kill -0 $cmd_pid 2>/dev/null; do
            if [ "$VERBOSE" = true ]; then
                echo "Waiting for process $cmd_pid to terminate..."
            fi
            sleep 0.5
        done
        
        # make sure we got all the child processes
        remaining_children=$(pgrep -P $cmd_pid 2>/dev/null)
        if [ ! -z "$remaining_children" ]; then
            if [ "$VERBOSE" = true ]; then
                echo "Killing remaining child processes: $remaining_children"
            fi
            kill -9 $remaining_children 2>/dev/null
        fi
        
        exit_code=124  
        echo "Command timed out after $TIMEOUT seconds (exit code: $exit_code)"
    else
        
        if [ "$EXIT_ON_SUCCESS" = true ] && [ $exit_code -eq 0 ]; then
            if [ "$VERBOSE" = true ]; then
                echo "Command completed successfully. Exiting as requested."
            fi
            exit 0
        fi
    fi
    
    # sleep 1
done

if [ "$VERBOSE" = true ]; then
    echo -e "\nAll iterations completed."
fi

exit 0