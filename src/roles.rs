use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    User = 0,
    Seller = 1,
    Admin = 2,
    Owner = 3,
}

pub fn role_of(db: &Db, user_id: i64, owner_id: i64) -> Role {
    if user_id == owner_id {
        return Role::Owner;
    }
    if db.is_admin(user_id, owner_id) {
        return Role::Admin;
    }
    if db.get_seller(user_id).is_some() {
        return Role::Seller;
    }
    Role::User
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use std::fs;

    #[test]
    fn test_role_of() {
        let path = "/tmp/opencode/test_role_of.db";
        let _ = fs::remove_file(path);
        let db = Db::new(path).unwrap();
        let owner_id = 999i64;
        // owner
        assert_eq!(role_of(&db, owner_id, owner_id), Role::Owner);
        // admin
        db.add_admin(100, Some("admin1"));
        assert_eq!(role_of(&db, 100, owner_id), Role::Admin);
        // seller
        db.upsert_seller(200, Some("seller1"), None);
        assert_eq!(role_of(&db, 200, owner_id), Role::Seller);
        // user
        assert_eq!(role_of(&db, 300, owner_id), Role::User);
    }

    #[test]
    fn test_role_owner() {
        let path = "/tmp/opencode/test_role_owner.db";
        let _ = fs::remove_file(path);
        let db = Db::new(path).unwrap();
        let owner_id = 777i64;
        assert_eq!(role_of(&db, owner_id, owner_id), Role::Owner);
    }

    #[test]
    fn test_role_admin() {
        let path = "/tmp/opencode/test_role_admin.db";
        let _ = fs::remove_file(path);
        let db = Db::new(path).unwrap();
        let owner_id = 999i64;
        db.add_admin(101, Some("admin1"));
        assert_eq!(role_of(&db, 101, owner_id), Role::Admin);
    }

    #[test]
    fn test_role_seller() {
        let path = "/tmp/opencode/test_role_seller.db";
        let _ = fs::remove_file(path);
        let db = Db::new(path).unwrap();
        let owner_id = 999i64;
        db.upsert_seller(201, Some("seller1"), None);
        assert_eq!(role_of(&db, 201, owner_id), Role::Seller);
    }

    #[test]
    fn test_role_user() {
        let path = "/tmp/opencode/test_role_user.db";
        let _ = fs::remove_file(path);
        let db = Db::new(path).unwrap();
        let owner_id = 999i64;
        assert_eq!(role_of(&db, 301, owner_id), Role::User);
    }

    #[test]
    fn test_role_ordering() {
        assert!(Role::Owner > Role::Admin);
        assert!(Role::Admin > Role::Seller);
        assert!(Role::Seller > Role::User);
    }
}
