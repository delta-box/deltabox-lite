/* Native WinFsp bridge. The SDK ABI stays in C; undo policy and persistence stay in Rust. */
#include <winfsp/winfsp.h>
#include <stdlib.h>
#include <wchar.h>
#include <stdio.h>

typedef NTSTATUS (*DB_ACTION)(void *);
extern NTSTATUS db_check(void *, const WCHAR *, int);
extern NTSTATUS db_mutate(void *, UINT32, const WCHAR *, const WCHAR *, UINT64, UINT64, DB_ACTION, void *, const HANDLE *);
extern void db_io_failed(void *);

typedef struct {
    FSP_FILE_SYSTEM *fs;
    void *engine;
    WCHAR *root;
    volatile LONG writers;
    BOOL strict;
} DB_FS;
typedef struct {
    HANDLE handle;
    DB_FS *owner;
    BOOL writer;
    BOOL directory;
    BOOL delete_pending;
} DB_FILE;

static HMODULE winfsp;
static NTSTATUS (*pStatus)(DWORD);
static NTSTATUS (*pCreate)(PWSTR,const FSP_FSCTL_VOLUME_PARAMS *,const FSP_FILE_SYSTEM_INTERFACE *,FSP_FILE_SYSTEM **);
static NTSTATUS (*pMount)(FSP_FILE_SYSTEM *,PWSTR);
static NTSTATUS (*pStart)(FSP_FILE_SYSTEM *,ULONG);
static void (*pStop)(FSP_FILE_SYSTEM *);
static void (*pDelete)(FSP_FILE_SYSTEM *);
static BOOLEAN (*pAddDir)(FSP_FSCTL_DIR_INFO *,PVOID,ULONG,PULONG);
static void (*pGuard)(FSP_FILE_SYSTEM *,FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY);

NTSTATUS db_status_from_win32(DWORD error) {
    if (pStatus) return pStatus(error);
    switch (error) {
    case ERROR_SUCCESS: return STATUS_SUCCESS;
    case ERROR_FILE_NOT_FOUND: return STATUS_OBJECT_NAME_NOT_FOUND;
    case ERROR_PATH_NOT_FOUND: return STATUS_OBJECT_PATH_NOT_FOUND;
    case ERROR_ACCESS_DENIED: return STATUS_ACCESS_DENIED;
    default: return STATUS_UNSUCCESSFUL;
    }
}
static NTSTATUS last_error(void) { return db_status_from_win32(GetLastError()); }
static DB_FS *owner(FSP_FILE_SYSTEM *fs) { return (DB_FS *)fs->UserContext; }
static BOOL writable(UINT32 access) {
    return 0 != (access & (FILE_WRITE_DATA|FILE_APPEND_DATA|FILE_WRITE_EA|FILE_WRITE_ATTRIBUTES|DELETE|WRITE_DAC|WRITE_OWNER));
}
static NTSTATUS load_winfsp(void) {
    if (winfsp) return STATUS_SUCCESS;
    WCHAR install[32768]; DWORD size=sizeof(install);
    LSTATUS result=RegGetValueW(HKEY_LOCAL_MACHINE,L"SOFTWARE\\WinFsp",L"InstallDir",
        RRF_RT_REG_SZ|RRF_SUBKEY_WOW6432KEY,NULL,install,&size);
    if (result != ERROR_SUCCESS) return db_status_from_win32(result);
#if defined(_M_ARM64)
    wcscat_s(install,32768,L"bin\\winfsp-a64.dll");
#elif defined(_WIN64)
    wcscat_s(install,32768,L"bin\\winfsp-x64.dll");
#else
    wcscat_s(install,32768,L"bin\\winfsp-x86.dll");
#endif
    winfsp=LoadLibraryExW(install,NULL,LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR|LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!winfsp) return last_error();
#define LOAD(field,name) do { *(FARPROC *)&field=GetProcAddress(winfsp,name); if (!field) return STATUS_PROCEDURE_NOT_FOUND; } while (0)
    LOAD(pStatus,"FspNtStatusFromWin32");
    LOAD(pCreate,"FspFileSystemCreate"); LOAD(pMount,"FspFileSystemSetMountPoint");
    LOAD(pStart,"FspFileSystemStartDispatcher"); LOAD(pStop,"FspFileSystemStopDispatcher");
    LOAD(pDelete,"FspFileSystemDelete"); LOAD(pAddDir,"FspFileSystemAddDirInfo");
    LOAD(pGuard,"FspFileSystemSetOperationGuardStrategyF");
#undef LOAD
    return STATUS_SUCCESS;
}

static NTSTATUS join_path(DB_FS *db,const WCHAR *name,WCHAR **path) {
    size_t r=wcslen(db->root), n=wcslen(name);
    *path=(WCHAR *)calloc(r+n+2,sizeof(WCHAR));
    if (!*path) return STATUS_INSUFFICIENT_RESOURCES;
    wcscpy_s(*path,r+n+2,db->root);
    if (name[0] && name[0] != L'\\') wcscat_s(*path,r+n+2,L"\\");
    wcscat_s(*path,r+n+2,name);
    return STATUS_SUCCESS;
}
static NTSTATUS make_path(DB_FS *db,const WCHAR *name,BOOL write,WCHAR **path) {
    NTSTATUS status=db_check(db->engine,name,write);
    return NT_SUCCESS(status) ? join_path(db,name,path) : status;
}

static NTSTATUS current_name(DB_FILE *file,WCHAR **name) {
    DWORD count=GetFinalPathNameByHandleW(file->handle,NULL,0,FILE_NAME_NORMALIZED|VOLUME_NAME_DOS);
    if (!count) return last_error();
    WCHAR *path=(WCHAR *)calloc(count+1,sizeof(WCHAR));
    if (!path) return STATUS_INSUFFICIENT_RESOURCES;
    if (!GetFinalPathNameByHandleW(file->handle,path,count+1,FILE_NAME_NORMALIZED|VOLUME_NAME_DOS)) { free(path); return last_error(); }
    size_t root=wcslen(file->owner->root);
    if (_wcsnicmp(path,file->owner->root,root) || (path[root] && path[root] != L'\\')) { free(path); return STATUS_ACCESS_DENIED; }
    *name=_wcsdup(path+root);
    free(path);
    return *name ? STATUS_SUCCESS : STATUS_INSUFFICIENT_RESOURCES;
}

static NTSTATUS file_info(HANDLE handle,FSP_FSCTL_FILE_INFO *info) {
    BY_HANDLE_FILE_INFORMATION i;
    FILE_STANDARD_INFO standard;
    if (!GetFileInformationByHandle(handle,&i) || !GetFileInformationByHandleEx(handle,FileStandardInfo,&standard,sizeof(standard))) return last_error();
    memset(info,0,sizeof(*info));
    info->FileAttributes=i.dwFileAttributes;
    info->AllocationSize=standard.AllocationSize.QuadPart;
    info->FileSize=standard.EndOfFile.QuadPart;
    info->CreationTime=((UINT64)i.ftCreationTime.dwHighDateTime<<32)|i.ftCreationTime.dwLowDateTime;
    info->LastAccessTime=((UINT64)i.ftLastAccessTime.dwHighDateTime<<32)|i.ftLastAccessTime.dwLowDateTime;
    info->LastWriteTime=((UINT64)i.ftLastWriteTime.dwHighDateTime<<32)|i.ftLastWriteTime.dwLowDateTime;
    info->ChangeTime=info->LastWriteTime;
    info->IndexNumber=((UINT64)i.nFileIndexHigh<<32)|i.nFileIndexLow;
    info->HardLinks=i.nNumberOfLinks;
    return STATUS_SUCCESS;
}

static NTSTATUS get_volume(FSP_FILE_SYSTEM *fs,FSP_FSCTL_VOLUME_INFO *info) {
    ULARGE_INTEGER free_bytes,total,available;
    if (!GetDiskFreeSpaceExW(owner(fs)->root,&available,&total,&free_bytes)) return last_error();
    memset(info,0,sizeof(*info)); info->TotalSize=total.QuadPart; info->FreeSize=available.QuadPart;
    const WCHAR *label=L"DeltaFS"; info->VolumeLabelLength=(UINT16)(wcslen(label)*sizeof(WCHAR));
    memcpy(info->VolumeLabel,label,info->VolumeLabelLength);
    return STATUS_SUCCESS;
}

static NTSTATUS security_path(const WCHAR *path,PSECURITY_DESCRIPTOR descriptor,SIZE_T *size) {
    DWORD needed=0;
    BOOL ok=GetFileSecurityW(path,OWNER_SECURITY_INFORMATION|GROUP_SECURITY_INFORMATION|DACL_SECURITY_INFORMATION,
        descriptor,descriptor ? (DWORD)*size : 0,&needed);
    if(size)*size=needed;
    return ok?STATUS_SUCCESS:last_error();
}

static NTSTATUS security_name(FSP_FILE_SYSTEM *fs,PWSTR name,PUINT32 attributes,PSECURITY_DESCRIPTOR descriptor,SIZE_T *size) {
    WCHAR *path; NTSTATUS status=make_path(owner(fs),name,FALSE,&path);
    if (!NT_SUCCESS(status)) return status;
    DWORD attr=GetFileAttributesW(path);
    if (attr == INVALID_FILE_ATTRIBUTES) status=last_error();
    else { if (attributes) *attributes=attr; if (size) status=security_path(path,descriptor,size); }
    free(path); return status;
}

typedef struct { DB_FS *db; PWSTR name; UINT32 options,access,attributes; PSECURITY_DESCRIPTOR sd; DB_FILE *file; } CREATE_ARGS;
static NTSTATUS create_action(void *opaque) {
    CREATE_ARGS *args=(CREATE_ARGS *)opaque; WCHAR *path;
    NTSTATUS status=join_path(args->db,args->name,&path); if (!NT_SUCCESS(status)) return status;
    BOOL directory=0 != (args->options&FILE_DIRECTORY_FILE);
    if (directory && !CreateDirectoryW(path,NULL)) { status=last_error(); free(path); return status; }
    HANDLE h=CreateFileW(path,args->access|(args->db->strict?GENERIC_WRITE:0),FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,
        NULL,directory ? OPEN_EXISTING : CREATE_NEW,
        args->attributes|FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OVERLAPPED|FILE_FLAG_OPEN_REPARSE_POINT|(args->db->strict?FILE_FLAG_WRITE_THROUGH:0),NULL);
    if (h == INVALID_HANDLE_VALUE) status=last_error();
    else { args->file->handle=h; args->file->directory=directory; }
    free(path); return status;
}

static NTSTATUS create_file(FSP_FILE_SYSTEM *fs,PWSTR name,UINT32 options,UINT32 access,UINT32 attributes,
    PSECURITY_DESCRIPTOR sd,UINT64 allocation,PVOID *context,FSP_FSCTL_FILE_INFO *info) {
    (void)allocation;
    DB_FILE *file=(DB_FILE *)calloc(1,sizeof(*file)); if (!file) return STATUS_INSUFFICIENT_RESOURCES;
    file->owner=owner(fs);
    CREATE_ARGS args={owner(fs),name,options,access,attributes,sd,file};
    NTSTATUS status=db_mutate(owner(fs)->engine,2,name,NULL,0,0,create_action,&args,&file->handle);
    if (!NT_SUCCESS(status)) { if (file->handle && file->handle != INVALID_HANDLE_VALUE) CloseHandle(file->handle); free(file); return status; }
    file->writer=writable(access); if (file->writer) InterlockedIncrement(&file->owner->writers);
    *context=file; return file_info(file->handle,info);
}

static NTSTATUS open_file(FSP_FILE_SYSTEM *fs,PWSTR name,UINT32 options,UINT32 access,PVOID *context,FSP_FSCTL_FILE_INFO *info) {
    (void)options;
    WCHAR *path; NTSTATUS status=make_path(owner(fs),name,writable(access),&path); if (!NT_SUCCESS(status)) return status;
    HANDLE h=CreateFileW(path,access,FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE,NULL,OPEN_EXISTING,
        FILE_FLAG_BACKUP_SEMANTICS|FILE_FLAG_OVERLAPPED|FILE_FLAG_OPEN_REPARSE_POINT|(owner(fs)->strict?FILE_FLAG_WRITE_THROUGH:0),NULL);
    free(path); if (h == INVALID_HANDLE_VALUE) return last_error();
    status=file_info(h,info);
    if (!NT_SUCCESS(status) || (writable(access) && info->HardLinks>1)) { CloseHandle(h); return NT_SUCCESS(status) ? STATUS_NOT_SUPPORTED : status; }
    DB_FILE *file=(DB_FILE *)calloc(1,sizeof(*file)); if (!file) { CloseHandle(h); return STATUS_INSUFFICIENT_RESOURCES; }
    file->handle=h; file->owner=owner(fs); file->directory=0 != (info->FileAttributes&FILE_ATTRIBUTE_DIRECTORY);
    file->writer=writable(access); if (file->writer) InterlockedIncrement(&file->owner->writers);
    *context=file; return STATUS_SUCCESS;
}

static void close_file(FSP_FILE_SYSTEM *fs,PVOID context) {
    (void)fs; DB_FILE *file=(DB_FILE *)context;
    if (!CloseHandle(file->handle) && file->owner->strict) db_io_failed(file->owner->engine);
    if (file->writer) InterlockedDecrement(&file->owner->writers);
    free(file);
}

static void cleanup(FSP_FILE_SYSTEM *fs,PVOID context,PWSTR name,ULONG flags) {
    (void)fs; (void)name; (void)flags;
    (void)context;
    /* Rust checks the data/metadata flush before APPLIED. The command boundary
       flushes surviving parents after all writer handles close (including
       delete-on-close). A void cleanup callback must not hide a failed flush. */
}

static NTSTATUS read_file(FSP_FILE_SYSTEM *fs,PVOID context,PVOID buffer,UINT64 offset,ULONG length,PULONG transferred) {
    (void)fs; DB_FILE *file=(DB_FILE *)context;
    OVERLAPPED ov={0}; ov.Offset=(DWORD)offset; ov.OffsetHigh=(DWORD)(offset>>32);
    ov.hEvent=CreateEventW(NULL,TRUE,FALSE,NULL); if (!ov.hEvent) return last_error();
    BOOL ok=ReadFile(file->handle,buffer,length,transferred,&ov);
    if (!ok && GetLastError()==ERROR_IO_PENDING) ok=GetOverlappedResult(file->handle,&ov,transferred,TRUE);
    NTSTATUS status=ok ? STATUS_SUCCESS : last_error(); CloseHandle(ov.hEvent); return status;
}

typedef struct { DB_FILE *file; PVOID buffer; UINT64 offset; ULONG length; PULONG transferred; } WRITE_ARGS;
static NTSTATUS write_action(void *opaque) {
    WRITE_ARGS *args=(WRITE_ARGS *)opaque;
    OVERLAPPED ov={0}; ov.Offset=(DWORD)args->offset; ov.OffsetHigh=(DWORD)(args->offset>>32);
    ov.hEvent=CreateEventW(NULL,TRUE,FALSE,NULL); if (!ov.hEvent) return last_error();
    BOOL ok=WriteFile(args->file->handle,args->buffer,args->length,args->transferred,&ov);
    if (!ok && GetLastError()==ERROR_IO_PENDING) ok=GetOverlappedResult(args->file->handle,&ov,args->transferred,TRUE);
    NTSTATUS status=ok ? STATUS_SUCCESS : last_error(); CloseHandle(ov.hEvent); return status;
}
static NTSTATUS write_file(FSP_FILE_SYSTEM *fs,PVOID context,PVOID buffer,UINT64 offset,ULONG length,
    BOOLEAN append,BOOLEAN constrained,PULONG transferred,FSP_FSCTL_FILE_INFO *info) {
    DB_FILE *file=(DB_FILE *)context; WCHAR *name;
    NTSTATUS status=current_name(file,&name); if (!NT_SUCCESS(status)) return status;
    LARGE_INTEGER size;
    if (!GetFileSizeEx(file->handle,&size)) { free(name); return last_error(); }
    if (append) offset=size.QuadPart;
    if (constrained) {
        if (offset>=(UINT64)size.QuadPart) { *transferred=0; free(name); return file_info(file->handle,info); }
        if (length>(UINT64)size.QuadPart-offset) length=(ULONG)((UINT64)size.QuadPart-offset);
    }
    WRITE_ARGS args={file,buffer,offset,length,transferred};
    status=db_mutate(owner(fs)->engine,1,name,NULL,offset,length,write_action,&args,&file->handle);
    free(name); return NT_SUCCESS(status) ? file_info(file->handle,info) : status;
}

typedef struct { DB_FILE *file; UINT64 size; BOOL allocation; } SIZE_ARGS;
static NTSTATUS size_action(void *opaque) {
    SIZE_ARGS *args=(SIZE_ARGS *)opaque;
    if (args->allocation) {
        FILE_ALLOCATION_INFO info; info.AllocationSize.QuadPart=args->size;
        return SetFileInformationByHandle(args->file->handle,FileAllocationInfo,&info,sizeof(info)) ? STATUS_SUCCESS : last_error();
    }
    FILE_END_OF_FILE_INFO info; info.EndOfFile.QuadPart=args->size;
    return SetFileInformationByHandle(args->file->handle,FileEndOfFileInfo,&info,sizeof(info)) ? STATUS_SUCCESS : last_error();
}
static NTSTATUS set_size(FSP_FILE_SYSTEM *fs,PVOID context,UINT64 size,BOOLEAN allocation,FSP_FSCTL_FILE_INFO *info) {
    DB_FILE *file=(DB_FILE *)context; WCHAR *name;
    NTSTATUS status=current_name(file,&name); if (!NT_SUCCESS(status)) return status;
    SIZE_ARGS args={file,size,allocation};
    status=db_mutate(owner(fs)->engine,3,name,NULL,size,0,size_action,&args,&file->handle);
    free(name); return NT_SUCCESS(status) ? file_info(file->handle,info) : status;
}
typedef struct {DB_FILE *file;UINT32 attributes;BOOLEAN replace;UINT64 allocation;} OVERWRITE_ARGS;
static NTSTATUS overwrite_action(void *opaque) {
    OVERWRITE_ARGS *args=(OVERWRITE_ARGS *)opaque;
    FILE_BASIC_INFO basic;
    if(!GetFileInformationByHandleEx(args->file->handle,FileBasicInfo,&basic,sizeof(basic)))return last_error();
    UINT32 attrs=args->replace?args->attributes:(basic.FileAttributes|args->attributes);
    memset(&basic,0,sizeof(basic));basic.FileAttributes=attrs|FILE_ATTRIBUTE_ARCHIVE;
    SIZE_ARGS size={args->file,0,FALSE};NTSTATUS status=size_action(&size);
    if(!NT_SUCCESS(status))return status;
    if(!SetFileInformationByHandle(args->file->handle,FileBasicInfo,&basic,sizeof(basic)))return last_error();
    size.size=args->allocation;size.allocation=TRUE;
    return size_action(&size);
}
static NTSTATUS overwrite(FSP_FILE_SYSTEM *fs,PVOID context,UINT32 attributes,BOOLEAN replace,UINT64 allocation,FSP_FSCTL_FILE_INFO *info) {
    DB_FILE *file=(DB_FILE *)context;WCHAR *name;
    NTSTATUS status=current_name(file,&name);if(!NT_SUCCESS(status))return status;
    OVERWRITE_ARGS args={file,attributes,replace,allocation};
    status=db_mutate(owner(fs)->engine,3,name,NULL,0,0,overwrite_action,&args,&file->handle);
    free(name);return NT_SUCCESS(status)?file_info(file->handle,info):status;
}

typedef struct { DB_FILE *file; FILE_BASIC_INFO basic; } BASIC_ARGS;
static NTSTATUS basic_action(void *opaque) {
    BASIC_ARGS *args=(BASIC_ARGS *)opaque;
    return SetFileInformationByHandle(args->file->handle,FileBasicInfo,&args->basic,sizeof(args->basic)) ? STATUS_SUCCESS : last_error();
}
static NTSTATUS set_basic(FSP_FILE_SYSTEM *fs,PVOID context,UINT32 attributes,UINT64 creation,UINT64 access,UINT64 write,UINT64 change,FSP_FSCTL_FILE_INFO *info) {
    DB_FILE *file=(DB_FILE *)context; WCHAR *name;
    NTSTATUS status=current_name(file,&name); if (!NT_SUCCESS(status)) return status;
    BASIC_ARGS args={0}; args.file=file;
    /* WinFsp strips NORMAL to zero; native FileBasicInfo uses zero for "keep". */
    args.basic.FileAttributes=attributes==INVALID_FILE_ATTRIBUTES ? 0 : (attributes?attributes:FILE_ATTRIBUTE_NORMAL);
    args.basic.CreationTime.QuadPart=creation; args.basic.LastAccessTime.QuadPart=access;
    args.basic.LastWriteTime.QuadPart=write; args.basic.ChangeTime.QuadPart=change;
    status=db_mutate(owner(fs)->engine,6,name,NULL,0,0,basic_action,&args,&file->handle);
    free(name); return NT_SUCCESS(status) ? file_info(file->handle,info) : status;
}

typedef struct { DB_FILE *file; BOOLEAN remove; } DELETE_ARGS;
static NTSTATUS delete_action(void *opaque) {
    DELETE_ARGS *args=(DELETE_ARGS *)opaque; FILE_DISPOSITION_INFO info={args->remove};
    return SetFileInformationByHandle(args->file->handle,FileDispositionInfo,&info,sizeof(info)) ? STATUS_SUCCESS : last_error();
}
static NTSTATUS set_delete(FSP_FILE_SYSTEM *fs,PVOID context,PWSTR name,BOOLEAN remove) {
    DB_FILE *file=(DB_FILE *)context;
    if (!remove) {
        // Cancelling deletion is safe: the retained undo restores the same object.
        DELETE_ARGS args={file,FALSE}; file->delete_pending=FALSE; return delete_action(&args);
    }
    if (file->delete_pending) return STATUS_SUCCESS;
    DELETE_ARGS args={file,TRUE};
    NTSTATUS status=db_mutate(owner(fs)->engine,4,name,NULL,0,0,delete_action,&args,&file->handle);
    if (NT_SUCCESS(status)) file->delete_pending=TRUE;
    return status;
}

typedef struct { DB_FILE *file; const WCHAR *target; BOOLEAN replace; } RENAME_ARGS;
static NTSTATUS rename_action(void *opaque) {
    RENAME_ARGS *args=(RENAME_ARGS *)opaque; WCHAR *path;
    NTSTATUS status=join_path(args->file->owner,args->target,&path); if (!NT_SUCCESS(status)) return status;
    size_t length=wcslen(path)*sizeof(WCHAR), size=sizeof(FILE_RENAME_INFO)+length;
    FILE_RENAME_INFO *info=(FILE_RENAME_INFO *)calloc(1,size);
    if (!info) { free(path); return STATUS_INSUFFICIENT_RESOURCES; }
    info->ReplaceIfExists=args->replace; info->FileNameLength=(DWORD)length; memcpy(info->FileName,path,length);
    status=SetFileInformationByHandle(args->file->handle,FileRenameInfo,info,(DWORD)size) ? STATUS_SUCCESS : last_error();
    free(info); free(path); return status;
}
static NTSTATUS rename_file(FSP_FILE_SYSTEM *fs,PVOID context,PWSTR name,PWSTR target,BOOLEAN replace) {
    if (!_wcsicmp(name,target)) return STATUS_NOT_SUPPORTED;
    RENAME_ARGS args={(DB_FILE *)context,target,replace};
    return db_mutate(owner(fs)->engine,5,name,target,0,0,rename_action,&args,&args.file->handle);
}

static NTSTATUS get_info(FSP_FILE_SYSTEM *fs,PVOID context,FSP_FSCTL_FILE_INFO *info) {
    (void)fs; return file_info(((DB_FILE *)context)->handle,info);
}
static NTSTATUS flush(FSP_FILE_SYSTEM *fs,PVOID context,FSP_FSCTL_FILE_INFO *info) {
    (void)fs; if (!context) return STATUS_SUCCESS;
    DB_FILE *file=(DB_FILE *)context;
    if (file->writer && !file->directory && !FlushFileBuffers(file->handle)) {
        NTSTATUS status=last_error();
        if (owner(fs)->strict) db_io_failed(owner(fs)->engine);
        return status;
    }
    return file_info(file->handle,info);
}
static NTSTATUS get_security(FSP_FILE_SYSTEM *fs,PVOID context,PSECURITY_DESCRIPTOR descriptor,SIZE_T *size) {
    (void)fs; DB_FILE *file=(DB_FILE *)context; WCHAR *name,*path;
    NTSTATUS status=current_name(file,&name); if (!NT_SUCCESS(status)) return status;
    status=make_path(file->owner,name,FALSE,&path); free(name); if (!NT_SUCCESS(status)) return status;
    status=security_path(path,descriptor,size); free(path); return status;
}

typedef struct { WIN32_FIND_DATAW data; } DIR_ENTRY;
static int compare_entries(const void *left,const void *right) {
    return CompareStringOrdinal(((const DIR_ENTRY *)left)->data.cFileName,-1,((const DIR_ENTRY *)right)->data.cFileName,-1,TRUE)-CSTR_EQUAL;
}
static NTSTATUS read_directory(FSP_FILE_SYSTEM *fs,PVOID context,PWSTR pattern,PWSTR marker,PVOID buffer,ULONG length,PULONG transferred) {
    (void)pattern; DB_FILE *file=(DB_FILE *)context; WCHAR *name,*path;
    NTSTATUS status=current_name(file,&name); if (!NT_SUCCESS(status)) return status;
    status=make_path(owner(fs),name,FALSE,&path); if (!NT_SUCCESS(status)) {free(name);return status;}
    size_t plen=wcslen(path), capacity=32,count=0;
    WCHAR *search=(WCHAR *)calloc(plen+3,sizeof(WCHAR));
    if (!search) {free(path);free(name);return STATUS_INSUFFICIENT_RESOURCES;}
    wcscpy_s(search,plen+3,path); wcscat_s(search,plen+3,L"\\*");
    DIR_ENTRY *entries=(DIR_ENTRY *)calloc(capacity,sizeof(DIR_ENTRY));
    WIN32_FIND_DATAW data; HANDLE find=FindFirstFileW(search,&data); free(search);
    if (!entries) { if(find!=INVALID_HANDLE_VALUE)FindClose(find);free(path);free(name);return STATUS_INSUFFICIENT_RESOURCES; }
    if (find!=INVALID_HANDLE_VALUE) {
        do {
            if (!wcscmp(data.cFileName,L".") || !wcscmp(data.cFileName,L"..")) continue;
            if (count==capacity) {
                capacity*=2; DIR_ENTRY *grown=(DIR_ENTRY *)realloc(entries,capacity*sizeof(DIR_ENTRY));
                if(!grown){status=STATUS_INSUFFICIENT_RESOURCES;break;} entries=grown;
            }
            entries[count++].data=data;
        } while (FindNextFileW(find,&data));
        FindClose(find);
    } else if (GetLastError()!=ERROR_FILE_NOT_FOUND) status=last_error();
    qsort(entries,count,sizeof(DIR_ENTRY),compare_entries);
    *transferred=0;
    BOOL exhausted=TRUE;
    for(size_t i=0;NT_SUCCESS(status) && i<count;i++) {
        WIN32_FIND_DATAW *d=&entries[i].data;
        if(marker && CompareStringOrdinal(d->cFileName,-1,marker,-1,TRUE)!=CSTR_GREATER_THAN) continue;
        size_t nlen=wcslen(name),dlen=wcslen(d->cFileName);
        WCHAR *child=(WCHAR *)calloc(nlen+dlen+2,sizeof(WCHAR)); if(!child){status=STATUS_INSUFFICIENT_RESOURCES;break;}
        wcscpy_s(child,nlen+dlen+2,name);wcscat_s(child,nlen+dlen+2,L"\\");wcscat_s(child,nlen+dlen+2,d->cFileName);
        NTSTATUS allowed=db_check(owner(fs)->engine,child,FALSE);free(child);if(!NT_SUCCESS(allowed))continue;
        union { FSP_FSCTL_DIR_INFO align; BYTE bytes[sizeof(FSP_FSCTL_DIR_INFO)+520]; } entry={0};
        FSP_FSCTL_DIR_INFO *di=(FSP_FSCTL_DIR_INFO *)entry.bytes;
        di->Size=(UINT16)(FIELD_OFFSET(FSP_FSCTL_DIR_INFO,FileNameBuf)+dlen*sizeof(WCHAR));
        memcpy(di->FileNameBuf,d->cFileName,dlen*sizeof(WCHAR));
        di->FileInfo.FileAttributes=d->dwFileAttributes;
        di->FileInfo.FileSize=((UINT64)d->nFileSizeHigh<<32)|d->nFileSizeLow;
        di->FileInfo.AllocationSize=(di->FileInfo.FileSize+4095)&~(UINT64)4095;
        di->FileInfo.CreationTime=((UINT64)d->ftCreationTime.dwHighDateTime<<32)|d->ftCreationTime.dwLowDateTime;
        di->FileInfo.LastAccessTime=((UINT64)d->ftLastAccessTime.dwHighDateTime<<32)|d->ftLastAccessTime.dwLowDateTime;
        di->FileInfo.LastWriteTime=((UINT64)d->ftLastWriteTime.dwHighDateTime<<32)|d->ftLastWriteTime.dwLowDateTime;
        di->FileInfo.ChangeTime=di->FileInfo.LastWriteTime;
        if(!pAddDir(di,buffer,length,transferred)){exhausted=FALSE;break;}
    }
    if(NT_SUCCESS(status) && exhausted)pAddDir(NULL,buffer,length,transferred);
    free(entries);free(path);free(name);return status;
}

static FSP_FILE_SYSTEM_INTERFACE db_interface={
    .GetVolumeInfo=get_volume,.GetSecurityByName=security_name,.Create=create_file,.Open=open_file,
    .Overwrite=overwrite,.Cleanup=cleanup,.Close=close_file,.Read=read_file,.Write=write_file,
    .Flush=flush,.GetFileInfo=get_info,.SetBasicInfo=set_basic,.SetFileSize=set_size,
    .Rename=rename_file,.GetSecurity=get_security,.ReadDirectory=read_directory,.SetDelete=set_delete
};

__declspec(dllexport) NTSTATUS db_mount(const WCHAR *root,const WCHAR *view,const WCHAR *prefix,void *engine,BOOL strict,void **result) {
    NTSTATUS status=load_winfsp(); if(!NT_SUCCESS(status))return status;
    DB_FS *db=(DB_FS *)calloc(1,sizeof(*db));if(!db)return STATUS_INSUFFICIENT_RESOURCES;
    db->root=_wcsdup(root);db->engine=engine;db->strict=strict;
    FSP_FSCTL_VOLUME_PARAMS params={0}; params.Version=sizeof(params);params.SectorSize=512;
    params.SectorsPerAllocationUnit=8;params.MaxComponentLength=255;params.FileInfoTimeout=0;
    params.CasePreservedNames=1;params.UnicodeOnDisk=1;params.PersistentAcls=1;
    params.FlushAndPurgeOnCleanup=1;params.UmFileContextIsUserContext2=1;
    wcscpy_s(params.FileSystemName,sizeof(params.FileSystemName)/sizeof(WCHAR),L"DeltaFS");
    /* Local MUP-backed volume: disk volumes reject PSEC clients before our
       callbacks. This prefix is local to WinFsp, not an SMB server. */
    wcscpy_s(params.Prefix,sizeof(params.Prefix)/sizeof(WCHAR),prefix);
    status=pCreate(L"" FSP_FSCTL_NET_DEVICE_NAME,&params,&db_interface,&db->fs);
    if(!NT_SUCCESS(status))fprintf(stderr,"WinFsp create: %08lx\n",(unsigned long)status);
    const WCHAR *mount_view=!wcsncmp(view,L"\\\\?\\",4)?view+4:view;
    WCHAR drive[3]={0};
    if(wcslen(mount_view)==3 && mount_view[1]==L':'){drive[0]=mount_view[0];drive[1]=L':';}
    if(NT_SUCCESS(status)){db->fs->UserContext=db;pGuard(db->fs,FSP_FILE_SYSTEM_OPERATION_GUARD_STRATEGY_COARSE);status=pMount(db->fs,drive[0]?drive:NULL);
        if(!NT_SUCCESS(status))fprintf(stderr,"WinFsp mount point: %08lx\n",(unsigned long)status);}
    if(NT_SUCCESS(status))status=pStart(db->fs,0);
    if(!NT_SUCCESS(status)){if(db->fs)pDelete(db->fs);free(db->root);free(db);return status;}
    *result=db;return STATUS_SUCCESS;
}
__declspec(dllexport) LONG db_writers(void *context) { return InterlockedCompareExchange(&((DB_FS *)context)->writers,0,0); }
const WCHAR *db_mountpoint(void *context) { return ((DB_FS *)context)->fs->MountPoint; }
__declspec(dllexport) void db_unmount(void *context) {
    DB_FS *db=(DB_FS *)context;pStop(db->fs);pDelete(db->fs);free(db->root);free(db);
}
