// nothp PROG ARGS...: disable THP for this process tree (inherited across execve), then exec PROG. Simulates THP=never.
#include <sys/prctl.h>
#include <unistd.h>
#include <stdio.h>
int main(int c, char **v){ if(prctl(PR_SET_THP_DISABLE,1,0,0,0)){perror("prctl");return 111;} execv(v[1],v+1); perror("execv"); return 112; }
