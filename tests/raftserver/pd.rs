#![allow(dead_code)]

use std::collections::{HashMap, BTreeMap, HashSet};
use std::vec::Vec;
use std::collections::Bound::{Included, Unbounded};

use tikv::proto::metapb;
use tikv::pd::{Client, Result, Error, Key};
use tikv::pd::errors::other;

#[derive(Default)]
struct Store {
    store: metapb::Store,
    region_ids: HashSet<u64>,
}

#[derive(Default)]
struct Node {
    node: metapb::Node,
    store_ids: HashSet<u64>,
}

impl Node {
    pub fn new(node: metapb::Node, stores: &[metapb::Store]) -> Node {
        let mut ids = HashSet::new();
        for v in stores {
            let id = v.get_store_id();
            ids.insert(id);
        }

        Node {
            node: node,
            store_ids: ids,
        }
    }
}

struct Cluster {
    cluster_id: u64,
    nodes: HashMap<u64, Node>,
    stores: HashMap<u64, Store>,
    regions: BTreeMap<Key, metapb::Region>,
    region_id_keys: HashMap<u64, Key>,
}

impl Cluster {
    pub fn new(cluster_id: u64,
               node: metapb::Node,
               stores: Vec<metapb::Store>,
               region: metapb::Region)
               -> Cluster {
        let mut c = Cluster {
            cluster_id: cluster_id,
            nodes: HashMap::new(),
            stores: HashMap::new(),
            regions: BTreeMap::new(),
            region_id_keys: HashMap::new(),
        };

        let node_id = node.get_node_id();
        c.nodes.insert(node_id, Node::new(node, &stores));

        // Now, some tests use multi peers in bootstrap,
        // disable this check.
        // TODO: enable this check later.
        // assert_eq!(region.get_peers().len(), 1);
        let end_key = region.get_end_key().to_vec();
        let first_store_id = region.get_peers()[0].get_store_id();

        for v in stores {
            let store_id = v.get_store_id();
            let mut store = Store {
                store: v,
                region_ids: HashSet::new(),
            };

            if store_id == first_store_id {
                store.region_ids.insert(region.get_region_id());
            }

            c.stores.insert(store_id, store);
        }

        c.region_id_keys.insert(region.get_region_id(), end_key.clone());
        c.regions.insert(end_key, region);

        c
    }

    fn put_node(&mut self, node: metapb::Node) -> Result<()> {
        let mut n = self.nodes.entry(node.get_node_id()).or_insert_with(Node::default);
        n.node = node;
        Ok(())
    }

    fn put_store(&mut self, store: metapb::Store) -> Result<()> {
        let mut n = self.nodes.get_mut(&store.get_node_id()).unwrap();
        let mut s = self.stores.entry(store.get_store_id()).or_insert_with(Store::default);
        n.store_ids.insert(store.get_store_id());
        s.store = store;
        Ok(())
    }

    fn delete_node(&mut self, node_id: u64) -> Result<()> {
        {
            let n = self.nodes.get(&node_id).unwrap();
            if !n.store_ids.is_empty() {
                return Err(Error::DeleteNotEmptyNode(node_id));
            }
        }

        self.nodes.remove(&node_id);
        Ok(())
    }

    fn delete_store(&mut self, store_id: u64) -> Result<()> {
        {
            let s = self.stores.get(&store_id).unwrap();
            if !s.region_ids.is_empty() {
                return Err(Error::DeleteNotEmptyStore(store_id));
            }

            let mut n = self.nodes.get_mut(&s.store.get_node_id()).unwrap();
            n.store_ids.remove(&store_id);
        }

        self.stores.remove(&store_id);

        Ok(())
    }

    fn get_node(&self, node_id: u64) -> Result<metapb::Node> {
        Ok(self.nodes.get(&node_id).unwrap().node.clone())
    }

    fn get_store(&self, store_id: u64) -> Result<metapb::Store> {
        Ok(self.stores.get(&store_id).unwrap().store.clone())
    }

    fn get_region(&self, key: &[u8]) -> Result<metapb::Region> {
        // must exist a region contains this key.
        let (_, region) = self.regions
                              .range::<Key, Key>(Included(&key.to_vec()), Unbounded)
                              .next()
                              .unwrap();

        Ok(region.clone())
    }

    fn get_region_by_id(&self, region_id: u64) -> Result<metapb::Region> {
        let key = self.region_id_keys.get(&region_id).unwrap();
        self.get_region(&key)
    }

    fn change_peer(&mut self, region: metapb::Region) -> Result<()> {
        let end_key = region.get_end_key().to_vec();
        if !self.regions.contains_key(&end_key) {
            return Err(other(format!("region {:?} doesn't exist", region)));
        }

        for peer in region.get_peers() {
            let store = self.stores.get_mut(&peer.get_store_id()).unwrap();
            store.region_ids.insert(region.get_region_id());
        }

        assert!(self.regions.insert(end_key, region).is_some());

        Ok(())
    }

    fn split_region(&mut self, left: metapb::Region, right: metapb::Region) -> Result<()> {
        let left_end_key = left.get_end_key().to_vec();
        let right_end_key = right.get_end_key().to_vec();

        // TODO: if we use column family later, the maximum end key is empty,
        // so we should use another way to check it.
        assert!(right_end_key > left_end_key);

        // origin pre-split region's end key is the same as right end key,
        // and must exists.
        if !self.regions.contains_key(&right_end_key) {
            return Err(other(format!("region {:?} doesn't exist", right)));
        }

        if self.regions.contains_key(&left_end_key) {
            return Err(other(format!("region {:?} has already existed", left)));
        }

        assert!(self.region_id_keys.insert(left.get_region_id(), left_end_key.clone()).is_some());
        assert!(self.region_id_keys.insert(right.get_region_id(), right_end_key.clone()).is_none());

        for peer in right.get_peers() {
            let store = self.stores.get_mut(&peer.get_store_id()).unwrap();
            store.region_ids.insert(right.get_region_id());
        }

        assert!(self.regions.insert(left_end_key, left).is_none());
        assert!(self.regions.insert(right_end_key, right).is_some());

        Ok(())
    }

    fn get_stores(&self) -> Vec<metapb::Store> {
        self.stores.values().map(|s| s.store.clone()).collect()
    }
}

pub struct PdClient {
    clusters: HashMap<u64, Cluster>,

    node_id: u64,
    store_id: u64,
    region_id: u64,
    peer_id: u64,
}

impl PdClient {
    pub fn new() -> PdClient {
        PdClient {
            clusters: HashMap::new(),
            // We use 1 for bootstrap in some tests,
            // so here use a larger base value to avoid conflict.
            node_id: 1000,
            store_id: 1000,
            region_id: 1000,
            peer_id: 1000,
        }
    }

    fn get_cluster(&self, cluster_id: u64) -> Result<&Cluster> {
        match self.clusters.get(&cluster_id) {
            None => Err(Error::ClusterNotBootstrapped(cluster_id)),
            Some(cluster) => Ok(cluster),
        }
    }

    fn get_mut_cluster(&mut self, cluster_id: u64) -> Result<&mut Cluster> {
        match self.clusters.get_mut(&cluster_id) {
            None => Err(Error::ClusterNotBootstrapped(cluster_id)),
            Some(cluster) => Ok(cluster),
        }
    }

    pub fn change_peer(&mut self, cluster_id: u64, region: metapb::Region) -> Result<()> {
        let mut cluster = try!(self.get_mut_cluster(cluster_id));
        cluster.change_peer(region)
    }

    pub fn split_region(&mut self,
                        cluster_id: u64,
                        left: metapb::Region,
                        right: metapb::Region)
                        -> Result<()> {
        let mut cluster = try!(self.get_mut_cluster(cluster_id));
        cluster.split_region(left, right)
    }

    pub fn get_stores(&self, cluster_id: u64) -> Result<Vec<metapb::Store>> {
        let cluster = try!(self.get_cluster(cluster_id));
        Ok(cluster.get_stores())
    }

    pub fn get_region_by_id(&self, cluster_id: u64, region_id: u64) -> Result<metapb::Region> {
        let cluster = try!(self.get_cluster(cluster_id));
        cluster.get_region_by_id(region_id)
    }
}

impl Client for PdClient {
    fn bootstrap_cluster(&mut self,
                         cluster_id: u64,
                         node: metapb::Node,
                         stores: Vec<metapb::Store>,
                         region: metapb::Region)
                         -> Result<()> {
        if self.is_cluster_bootstrapped(cluster_id).unwrap() {
            return Err(Error::ClusterBootstrapped(cluster_id));
        }

        self.clusters.insert(cluster_id, Cluster::new(cluster_id, node, stores, region));

        Ok(())
    }

    fn is_cluster_bootstrapped(&self, cluster_id: u64) -> Result<bool> {
        Ok(self.clusters.contains_key(&cluster_id))
    }

    fn alloc_node_id(&mut self) -> Result<u64> {
        self.node_id += 1;
        Ok(self.node_id)
    }

    fn alloc_store_id(&mut self) -> Result<u64> {
        self.store_id += 1;
        Ok(self.store_id)
    }

    fn alloc_peer_id(&mut self) -> Result<u64> {
        self.peer_id += 1;
        Ok(self.peer_id)
    }

    fn alloc_region_id(&mut self) -> Result<u64> {
        self.region_id += 1;
        Ok(self.region_id)
    }

    fn put_node(&mut self, cluster_id: u64, node: metapb::Node) -> Result<()> {
        let mut cluster = try!(self.get_mut_cluster(cluster_id));
        cluster.put_node(node)
    }

    fn put_store(&mut self, cluster_id: u64, store: metapb::Store) -> Result<()> {
        let mut cluster = try!(self.get_mut_cluster(cluster_id));
        cluster.put_store(store)
    }

    fn delete_node(&mut self, cluster_id: u64, node_id: u64) -> Result<()> {
        let mut cluster = try!(self.get_mut_cluster(cluster_id));
        cluster.delete_node(node_id)
    }

    fn delete_store(&mut self, cluster_id: u64, store_id: u64) -> Result<()> {
        let mut cluster = try!(self.get_mut_cluster(cluster_id));
        cluster.delete_store(store_id)
    }

    fn get_node(&self, cluster_id: u64, node_id: u64) -> Result<metapb::Node> {
        let cluster = try!(self.get_cluster(cluster_id));
        cluster.get_node(node_id)
    }

    fn get_store(&self, cluster_id: u64, store_id: u64) -> Result<metapb::Store> {
        let cluster = try!(self.get_cluster(cluster_id));
        cluster.get_store(store_id)
    }


    fn get_region(&self, cluster_id: u64, key: &[u8]) -> Result<metapb::Region> {
        let cluster = try!(self.get_cluster(cluster_id));
        cluster.get_region(key)
    }
}