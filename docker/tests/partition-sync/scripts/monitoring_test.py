#!/usr/bin/env python3
"""
Simple monitoring test that observes the cluster state
without requiring transaction posting
"""

import asyncio
import aiohttp
import json
import time
import logging
from typing import Dict, List
from dataclasses import dataclass

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)

@dataclass
class NodeConfig:
    name: str
    url: str
    reward_address: str

@dataclass 
class ChunkCounts:
    data: int
    packed: int

class IrysMonitorClient:
    def __init__(self):
        self.nodes = {
            'test-irys-1': NodeConfig('test-irys-1', 'http://localhost:9080', 
                               '0x10605A299777D44BE5373D120d5479f07860325d'),
            'test-irys-2': NodeConfig('test-irys-2', 'http://localhost:9081',
                               '0x951AA3768fE8d51DbC348520b0825C0c1f197277'),
            'test-irys-3': NodeConfig('test-irys-3', 'http://localhost:9082',
                               '0x852BB3678fE8d51DbC348520b0825C0c1f197288')
        }
        self.session = None
    
    async def __aenter__(self):
        self.session = aiohttp.ClientSession()
        return self
    
    async def __aexit__(self, exc_type, exc_val, exc_tb):
        await self.session.close()

    async def get_node_info(self, node_name: str) -> Dict:
        """Get node information"""
        node = self.nodes[node_name]
        async with self.session.get(f"{node.url}/v1/info") as resp:
            resp.raise_for_status()
            return await resp.json()

    async def get_chain_height(self, node_name: str) -> int:
        """Get current chain height"""
        node = self.nodes[node_name]
        async with self.session.get(f"{node.url}/v1/observability/chain/height") as resp:
            resp.raise_for_status()
            data = await resp.json()
            return data['height']

    async def get_storage_intervals(self, node_name: str, ledger: str, slot_index: int, chunk_type: str) -> Dict:
        """Get storage intervals for monitoring sync"""
        node = self.nodes[node_name]
        url = f"{node.url}/v1/observability/storage/intervals/{ledger}/{slot_index}/{chunk_type}"
        
        try:
            async with self.session.get(url) as resp:
                if resp.status == 200:
                    return await resp.json()
                else:
                    return {"intervals": []}
        except Exception:
            return {"intervals": []}

    async def check_chunk_counts(self, node_name: str, ledger: str, slot_index: int) -> ChunkCounts:
        """Check data and packed chunk counts for a node/ledger/slot"""
        data_intervals = await self.get_storage_intervals(node_name, ledger, slot_index, "Data")
        packed_intervals = await self.get_storage_intervals(node_name, ledger, slot_index, "Entropy")
        
        # Count chunks from intervals
        data_count = sum(interval.get("end", 0) - interval.get("start", 0) + 1 
                        for interval in data_intervals.get("intervals", []) if interval.get("start") is not None)
        packed_count = sum(interval.get("end", 0) - interval.get("start", 0) + 1 
                          for interval in packed_intervals.get("intervals", []) if interval.get("start") is not None)
        
        return ChunkCounts(data=data_count, packed=packed_count)

    async def get_partition_summary(self, node_name: str, ledger: str = "Publish") -> Dict:
        """Get partition summary for a ledger"""
        node = self.nodes[node_name]
        # Try different node IDs to find active partitions
        for node_id in range(3):  # Try node IDs 0, 1, 2
            url = f"{node.url}/v1/observability/ledger/{ledger.lower()}/{node_id}/summary"
            try:
                async with self.session.get(url) as resp:
                    if resp.status == 200:
                        data = await resp.json()
                        if data:  # If we get non-empty data, return it
                            return {"node_id": node_id, "data": data}
            except Exception:
                pass
        return {}

async def run_simple_monitoring():
    """Simple monitoring test that observes cluster state"""
    async with IrysMonitorClient() as client:
        logger.info("🚀 Starting Simple Cluster Monitoring Test")
        
        # Phase 1: Connectivity Check
        logger.info("\n📋 Phase 1: Network Connectivity")
        
        nodes_connected = True
        for node_name in client.nodes:
            try:
                info = await client.get_node_info(node_name)
                logger.info(f"✅ {node_name}: connected, version={info.get('version', 'unknown')}")
            except Exception as e:
                logger.error(f"❌ Failed to connect to {node_name}: {e}")
                nodes_connected = False
        
        if not nodes_connected:
            logger.error("❌ Not all nodes are accessible")
            return False
        
        # Phase 2: Chain Height Monitoring
        logger.info("\n📋 Phase 2: Chain Height Monitoring")
        
        initial_heights = {}
        for node_name in client.nodes:
            try:
                height = await client.get_chain_height(node_name)
                initial_heights[node_name] = height
                logger.info(f"📊 {node_name} height: {height}")
            except Exception as e:
                logger.error(f"❌ Failed to get height for {node_name}: {e}")
                initial_heights[node_name] = 0
        
        # Check if nodes are at similar heights
        height_values = list(initial_heights.values())
        if height_values:
            max_diff = max(height_values) - min(height_values)
            if max_diff <= 2:
                logger.info(f"✅ Nodes are synchronized (max height difference: {max_diff})")
            else:
                logger.warning(f"⚠️  Nodes may be out of sync (height difference: {max_diff})")
        
        # Phase 3: Storage State Analysis
        logger.info("\n📋 Phase 3: Storage State Analysis")
        
        total_chunks = {"data": 0, "packed": 0}
        
        for node_name in client.nodes:
            logger.info(f"\n📦 {node_name} storage state:")
            node_total = {"data": 0, "packed": 0}
            
            # Check different ledger/slot combinations
            for ledger in ["Publish", "Submit"]:
                for slot in range(3):  # Check first 3 slots
                    try:
                        counts = await client.check_chunk_counts(node_name, ledger, slot)
                        if counts.data > 0 or counts.packed > 0:
                            logger.info(f"   {ledger}({slot}): data={counts.data}, packed={counts.packed}")
                            node_total["data"] += counts.data
                            node_total["packed"] += counts.packed
                    except Exception as e:
                        pass
            
            logger.info(f"   Total: data={node_total['data']}, packed={node_total['packed']}")
            total_chunks["data"] += node_total["data"]
            total_chunks["packed"] += node_total["packed"]
        
        # Phase 4: Partition Summary
        logger.info("\n📋 Phase 4: Partition Summaries")
        
        for node_name in client.nodes:
            for ledger in ["Publish", "Submit"]:
                summary = await client.get_partition_summary(node_name, ledger)
                if summary:
                    logger.info(f"{node_name} - {ledger} ledger (node_id={summary['node_id']}):")
                    logger.info(f"   {json.dumps(summary['data'], indent=4)}")
        
        # Phase 5: Monitor Chain Progression
        logger.info("\n📋 Phase 5: Chain Progression Monitoring (30 seconds)")
        
        start_time = time.time()
        monitoring_duration = 30
        check_interval = 5
        
        while time.time() - start_time < monitoring_duration:
            await asyncio.sleep(check_interval)
            
            logger.info(f"\n⏱️  Progress check at {int(time.time() - start_time)}s:")
            current_heights = {}
            
            for node_name in client.nodes:
                try:
                    height = await client.get_chain_height(node_name)
                    current_heights[node_name] = height
                    height_change = height - initial_heights.get(node_name, 0)
                    logger.info(f"   {node_name}: height={height} (Δ{height_change})")
                except Exception:
                    pass
        
        # Final Summary
        logger.info("\n🏁 MONITORING TEST SUMMARY:")
        
        final_heights = {}
        for node_name in client.nodes:
            try:
                height = await client.get_chain_height(node_name)
                final_heights[node_name] = height
                total_change = height - initial_heights.get(node_name, 0)
                logger.info(f"📊 {node_name} final: {height} (changed by {total_change})")
            except Exception:
                pass
        
        # Determine test success
        chain_progressed = any(final_heights.get(n, 0) > initial_heights.get(n, 0) 
                              for n in client.nodes)
        
        if nodes_connected and len(final_heights) == len(client.nodes):
            if chain_progressed:
                logger.info("✅ SUCCESS: Cluster is healthy and chain is progressing")
            else:
                logger.info("✅ SUCCESS: Cluster is healthy (chain stable)")
            
            if total_chunks["data"] > 0 or total_chunks["packed"] > 0:
                logger.info(f"💾 Data detected: {total_chunks['data']} data chunks, {total_chunks['packed']} packed chunks")
            
            return True
        else:
            logger.error("❌ FAILED: Cluster health check failed")
            return False

def main():
    """Main entry point"""
    import sys
    
    try:
        success = asyncio.run(run_simple_monitoring())
        sys.exit(0 if success else 1)
    except KeyboardInterrupt:
        logger.info("\nTest interrupted by user")
        sys.exit(130)
    except Exception as e:
        logger.error(f"Test failed with error: {e}")
        sys.exit(1)

if __name__ == "__main__":
    main()
