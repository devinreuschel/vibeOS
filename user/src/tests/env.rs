//! User-mode environment of DESIGN §11.4 (ROADMAP §11.6). The instructions
//! live in `arch`; this suite only names the case.

use vibeos_user::arch::user_env;
use vibeos_user::utest::Runner;

pub fn run(t: &mut Runner) {
    t.case("user_env", user_env);
}
