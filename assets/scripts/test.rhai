// Test script for the devroom goblin: trails the player, stopping at a
// polite distance, and strikes up a conversation when the player is
// close. Its wait-mode bubble sits top-center of the screen; waiting()
// pauses this script until the player presses Z to dismiss it, and the
// goblin starts over.
fn on_update(x, z, player_x, player_z, dt) {
    if waiting() { return (); }
    let dx = player_x - x;
    let dz = player_z - z;
    let dist = sqrt(dx*dx + dz*dz);
    if dist >= 2.0 {
        let speed = 1.5;
        let step = min(speed * dt, dist - 2.0);
        return [x + dx / dist * step, z + dz / dist * step];
    }
    say("Oh! You can see me?", #{wait: true, at: [160.0, 40.0]});
    ()
}
