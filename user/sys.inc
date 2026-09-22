; Shared syscall numbers. Linux x86_64. SYS_PSINFO is vibeOS-specific.

SYS_READ      equ 0
SYS_WRITE     equ 1
SYS_OPEN      equ 2
SYS_CLOSE     equ 3
SYS_SCHED_YIELD equ 24
SYS_DUP       equ 32
SYS_DUP2      equ 33
SYS_GETPID    equ 39
SYS_FORK      equ 57
SYS_EXECVE    equ 59
SYS_EXIT      equ 60
SYS_WAIT4     equ 61
SYS_KILL      equ 62
SYS_GETPPID   equ 110
SYS_PSINFO    equ 500

EFAULT        equ 14
EBADF         equ 9
EAGAIN        equ 11
ECHILD        equ 10

SIGKILL       equ 9
SIGSEGV       equ 11
