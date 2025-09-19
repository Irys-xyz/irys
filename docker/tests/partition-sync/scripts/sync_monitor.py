#!/usr/bin/env python3
"""Real-time monitoring of partition data sync across cluster"""

import asyncio
import curses
import sys
import os
sys.path.append(os.path.dirname(os.path.abspath(__file__)))

from partition_sync_test import IrysComprehensiveTestClient as IrysTestClient, ChunkCounts

class SyncMonitor:
    def __init__(self):
        self.client = None
        self.running = False
        
    async def monitor_sync_status(self, stdscr):
        """Real-time monitoring with curses display"""
        curses.curs_set(0)
        stdscr.nodelay(1)
        stdscr.timeout(1000)
        
        async with IrysTestClient() as client:
            self.client = client
            self.running = True
            
            while self.running:
                stdscr.clear()
                
                # Header
                stdscr.addstr(0, 0, "Irys Cluster Partition Data Sync Monitor", curses.A_BOLD)
                stdscr.addstr(1, 0, "=" * 60)
                
                # Node status
                row = 3
                for node_name in client.nodes:
                    try:
                        height = await client.get_chain_height(node_name)
                        info = await client.get_node_info(node_name)
                        
                        stdscr.addstr(row, 0, f"{node_name}:")
                        stdscr.addstr(row, 15, f"Height: {height}")
                        stdscr.addstr(row, 30, f"Peers: {info.get('peers', 0)}")
                        
                        # Check partition data
                        pub_counts = await client.check_chunk_counts(node_name, "Publish", 0)
                        sub0_counts = await client.check_chunk_counts(node_name, "Submit", 0)
                        sub1_counts = await client.check_chunk_counts(node_name, "Submit", 1)
                        
                        stdscr.addstr(row + 1, 2, f"Pub(0): D={pub_counts.data:3d} P={pub_counts.packed:3d}")
                        stdscr.addstr(row + 1, 25, f"Sub(0): D={sub0_counts.data:3d} P={sub0_counts.packed:3d}")
                        stdscr.addstr(row + 1, 48, f"Sub(1): D={sub1_counts.data:3d} P={sub1_counts.packed:3d}")
                        
                        # Check sync status
                        node_synced = (
                            pub_counts.data == 50 and pub_counts.packed == 10 and
                            sub0_counts.data == 50 and sub0_counts.packed == 10 and
                            sub1_counts.packed == 60 and sub1_counts.data == 0
                        )
                        
                        if node_synced:
                            stdscr.addstr(row + 2, 2, "Status: SYNCED", curses.A_BOLD | curses.color_pair(1))
                        else:
                            stdscr.addstr(row + 2, 2, "Status: SYNCING...", curses.color_pair(2))
                        
                        row += 4
                        
                    except Exception as e:
                        stdscr.addstr(row, 0, f"{node_name}: ERROR - {str(e)[:40]}", curses.color_pair(3))
                        row += 2
                
                # Legend
                stdscr.addstr(row + 1, 0, "Legend: D=Data chunks, P=Packed chunks")
                stdscr.addstr(row + 2, 0, "Expected: Pub(0): D=50 P=10, Sub(0): D=50 P=10, Sub(1): D=0 P=60")
                stdscr.addstr(row + 4, 0, "Press 'q' to quit, 'r' to refresh immediately")
                
                stdscr.refresh()
                
                # Check for quit
                key = stdscr.getch()
                if key == ord('q'):
                    self.running = False
                elif key == ord('r'):
                    continue  # Immediate refresh
                
                await asyncio.sleep(2)

def main():
    # Initialize colors
    def init_colors():
        curses.start_color()
        curses.init_pair(1, curses.COLOR_GREEN, curses.COLOR_BLACK)  # Synced
        curses.init_pair(2, curses.COLOR_YELLOW, curses.COLOR_BLACK)  # Syncing
        curses.init_pair(3, curses.COLOR_RED, curses.COLOR_BLACK)  # Error
    
    monitor = SyncMonitor()
    
    def run_monitor(stdscr):
        init_colors()
        asyncio.run(monitor.monitor_sync_status(stdscr))
    
    curses.wrapper(run_monitor)

if __name__ == "__main__":
    main()