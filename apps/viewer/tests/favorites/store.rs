use inventory::{LoginLandmark, LoginLandmarkCache};
use network::Grid;
use tempfile::tempdir;
use uuid::Uuid;

#[tokio::test]
async fn scopes_landmarks_by_grid_and_agent_and_preserves_order() {
    let dir = tempdir().expect("tempdir");
    let (cache, _) = LoginLandmarkCache::open_at_root(dir.path()).expect("open login landmark cache");
    let agni_agent = Uuid::from_u128(1);
    let aditi_agent = Uuid::from_u128(2);
    let agni_landmarks = vec![
        LoginLandmark { name: "Sandbox".into(), slurl: "secondlife://Agni%20region/128/64/23".into() },
        LoginLandmark { name: "Plaza".into(), slurl: "secondlife://Ahern/10/20/30".into() },
    ];
    let aditi_landmarks = vec![LoginLandmark { name: "Home".into(), slurl: "secondlife://Aditi%20region/1/2/3".into() }];

    cache.replace(Grid::Agni.id(), agni_agent, &agni_landmarks).await.expect("replace Agni landmarks");
    cache.replace(Grid::Aditi.id(), aditi_agent, &aditi_landmarks).await.expect("replace Aditi landmarks");

    assert_eq!(cache.load(Grid::Agni.id(), agni_agent).await.expect("load Agni"), agni_landmarks);
    assert_eq!(cache.load(Grid::Aditi.id(), aditi_agent).await.expect("load Aditi"), aditi_landmarks);
    assert!(cache.load(Grid::Agni.id(), aditi_agent).await.expect("cross-grid isolation").is_empty());

    cache.clear_all().await.expect("clear all landmarks");
    assert!(cache.load(Grid::Agni.id(), agni_agent).await.expect("Agni cleared").is_empty());
    assert!(cache.load(Grid::Aditi.id(), aditi_agent).await.expect("Aditi cleared").is_empty());

    cache.close().expect("close cache");
}
