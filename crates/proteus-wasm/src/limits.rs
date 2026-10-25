//! One allocation budget for all guest memories and tables in a store.
use wasmtime::{ResourceLimiter, Result};

pub(crate) struct Limits {
    budget: usize,
    used: usize,
    memory_reservation: usize,
    table_reservation: usize,
}

impl Limits {
    pub(crate) fn new(budget: usize) -> Self {
        Self { budget, used: 0, memory_reservation: 0, table_reservation: 0 }
    }

    fn reserve(&mut self, bytes: usize) -> bool {
        if bytes > self.budget.saturating_sub(self.used) {
            return false;
        }
        self.used += bytes;
        true
    }
}

impl ResourceLimiter for Limits {
    fn memory_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> Result<bool> {
        self.memory_reservation = 0;
        if maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        let delta = desired.saturating_sub(current);
        let allowed = self.reserve(delta);
        if allowed {
            self.memory_reservation = delta;
        }
        Ok(allowed)
    }

    fn memory_grow_failed(&mut self, _: wasmtime::Error) -> Result<()> {
        self.used -= std::mem::take(&mut self.memory_reservation);
        Ok(())
    }

    fn table_growing(&mut self, current: usize, desired: usize, maximum: Option<usize>) -> Result<bool> {
        self.table_reservation = 0;
        if desired > 1 << 20 || maximum.is_some_and(|max| desired > max) {
            return Ok(false);
        }
        // Wasmtime table slots occupy one pointer each. Charge them to the
        // same budget as linear memories, across every core instance.
        let Some(delta) = desired.saturating_sub(current).checked_mul(std::mem::size_of::<usize>()) else {
            return Ok(false);
        };
        let allowed = self.reserve(delta);
        if allowed {
            self.table_reservation = delta;
        }
        Ok(allowed)
    }

    fn table_grow_failed(&mut self, _: wasmtime::Error) -> Result<()> {
        self.used -= std::mem::take(&mut self.table_reservation);
        Ok(())
    }

    fn instances(&self) -> usize {
        32
    }
    fn memories(&self) -> usize {
        32
    }
    fn tables(&self) -> usize {
        64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::{Engine, Instance, Memory, MemoryType, Module, Ref, RefType, Store, Table, TableType};

    #[test]
    fn multiple_memories_and_tables_share_one_budget() {
        let engine = Engine::default();
        let mut store = Store::new(&engine, Limits::new(3 * 65536));
        store.limiter(|s| s);
        let a = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
        let _b = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
        let table = Table::new(&mut store, TableType::new(RefType::FUNCREF, 1, None), Ref::Func(None)).unwrap();
        assert!(a.grow(&mut store, 1).is_err(), "table and second memory count against the budget");
        assert!(Memory::new(&mut store, MemoryType::new(1, None)).is_err());
        assert!(table.grow(&mut store, 65536, Ref::Func(None)).is_err());
        // Rejected growth must not poison the remaining budget.
        table.grow(&mut store, 1, Ref::Func(None)).unwrap();
    }

    #[test]
    fn zero_size_objects_still_have_count_limits() {
        let engine = Engine::default();
        let mut store = Store::new(&engine, Limits::new(65536));
        store.limiter(|s| s);
        // Counts apply to guest module definitions, not embedding-created
        // Memory/Table handles. These modules define zero-length objects.
        let mut memories = b"\0asm\x01\0\0\0".to_vec();
        memories.extend([5, 67, 33]); // memory section: 33 definitions
        memories.extend([0, 0].repeat(33)); // no maximum, initial size 0
        let module = Module::from_binary(&engine, &memories).unwrap();
        assert!(Instance::new(&mut store, &module, &[]).is_err());

        let mut tables = b"\0asm\x01\0\0\0".to_vec();
        tables.extend([4, 0xc4, 1, 65]); // table section, 196-byte payload
        tables.extend([0x70, 0, 0].repeat(65)); // funcref, no maximum, initial size 0
        let module = Module::from_binary(&engine, &tables).unwrap();
        assert!(Instance::new(&mut store, &module, &[]).is_err());
    }

    #[test]
    fn failed_allocations_release_reservations_and_maxima_do_not_charge() {
        let mut limits = Limits::new(65536);
        assert!(!limits.memory_growing(0, 65536, Some(0)).unwrap());
        assert!(limits.memory_growing(0, 65536, None).unwrap());
        limits.memory_grow_failed(wasmtime::Error::msg("allocation failure")).unwrap();
        assert!(limits.table_growing(0, 100, None).unwrap());
        limits.table_grow_failed(wasmtime::Error::msg("allocation failure")).unwrap();
        assert!(limits.memory_growing(0, 65536, None).unwrap());
        assert!(!limits.memory_growing(0, 65536, None).unwrap());
        assert!(!Limits::new(usize::MAX).table_growing(0, (1 << 20) + 1, None).unwrap());
    }
}
