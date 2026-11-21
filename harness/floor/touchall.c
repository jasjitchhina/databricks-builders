#include <time.h>
// mmap every file under /data/{events,clicks,links} and touch one byte per page, then exit (measures fault + teardown)
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <stdlib.h>
int main(void){ const char *ds[]={"/data/events","/data/clicks","/data/links"}; volatile unsigned long s=0; long tot=0;
 int unmap = getenv("UNMAP")!=0;
 for(int d=0;d<3;d++){ DIR*D=opendir(ds[d]); struct dirent*e; if(!D)continue; while((e=readdir(D))){ if(e->d_name[0]=='.')continue; char p[512]; snprintf(p,512,"%s/%s",ds[d],e->d_name);
  int fd=open(p,O_RDONLY); struct stat st; fstat(fd,&st); if(st.st_size>0){ unsigned char*m=mmap(0,st.st_size,PROT_READ,MAP_SHARED,fd,0); for(long o=0;o<st.st_size;o+=4096)s+=m[o]; tot+=st.st_size; if(unmap)munmap(m,st.st_size);} close(fd);} closedir(D);}
 struct timespec ts; clock_gettime(CLOCK_REALTIME,&ts); fprintf(stderr,"MARK %ld.%09ld %ld\n",ts.tv_sec,ts.tv_nsec,tot); _exit(0);}
