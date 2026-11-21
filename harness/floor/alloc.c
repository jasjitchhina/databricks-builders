#include <time.h>
// touch GB gigabytes of anonymous memory (THP=1: madvise hugepage), then _exit: measures exit-time page freeing
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <stdio.h>
#include <pthread.h>
static size_t per; static int thp;
static void* w(void*a){ char*p=mmap(0,per,PROT_READ|PROT_WRITE,MAP_PRIVATE|MAP_ANONYMOUS,-1,0); if(thp)madvise(p,per,MADV_HUGEPAGE); for(size_t o=0;o<per;o+=4096)p[o]=1; return p;}
int main(void){ double gb=atof(getenv("GB")?getenv("GB"):"4"); thp=getenv("THP")!=0; int T=8; per=(size_t)(gb*(1UL<<30))/T; pthread_t t[8];
 for(int i=0;i<T;i++)pthread_create(&t[i],0,w,0); for(int i=0;i<T;i++)pthread_join(t[i],0);
 struct timespec ts; clock_gettime(CLOCK_REALTIME,&ts); fprintf(stderr,"MARK %ld.%09ld\n",ts.tv_sec,ts.tv_nsec); if(getenv("UNMAPFIRST")){} _exit(0);}
